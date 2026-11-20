#[allow(unused_imports)]
use super::*;

/// A class name may hold `)` (JVMS 4.2.2). Every descriptor scan in this
/// file used to take the first or the last `)` as the end of the
/// parameter list; `(LA)B;)LC)D;` has one parameter `A)B` and returns
/// `C)D`, which both scans misread.
#[test]
fn descriptor_scans_are_not_fooled_by_a_paren_in_a_class_name() {
    let d = "(LA)B;)LC)D;";
    assert_eq!(desc_close_paren(d), Some(6));
    assert_eq!(desc_close_paren("()V"), Some(1));
    assert_eq!(desc_close_paren("([[I[LA)B;J)Z"), Some(11));
    assert_eq!(desc_close_paren("(LA;"), None);
    // Malformed (`[` with no element type): the walk consumes the `)` as
    // the element and never closes, the same answer the jit-api walk gives.
    assert_eq!(desc_close_paren("([)V"), None);
    assert_eq!(desc_close_paren(""), None);
    assert_eq!(return_type_desc(d), "LC)D;");
    assert_eq!(descriptor_return_internal_name(d).as_deref(), Some("C)D"));
    assert_eq!(count_descriptor_params(d), 1);
    assert_eq!(count_descriptor_params("(LA)B;LC;I)V"), 3);
    assert!(desc_has_two_params("(LA)B;I)V"));
    assert!(!desc_has_two_params("(LA)B;)V"));
    // getfield (ref_kind 1) reads the return type, putfield (3) the last
    // parameter.
    assert_eq!(field_type_from_desc("(LO;)LC)D;", 1), "LC)D;");
    assert_eq!(field_type_from_desc("(LO;LC)D;)V", 3), "LC)D;");
    assert_eq!(
        descriptor_without_first_param("(LA)B;LC)D;)V").as_deref(),
        Some("(LC)D;)V")
    );
}

/// `filterArguments(address, 0, ofBuffer)` — Netty's
/// `PlatformDependent0.MEMORY_SEGMENT_ADDRESS_OF_BUFFER` — is `(Buffer)long`,
/// not the target's `(MemorySegment)long`.
#[test]
fn filter_arguments_adapter_takes_each_filters_parameter_type() {
    let seg = "Ljava/lang/foreign/MemorySegment;";
    let ofbuf = Some(format!("(Ljava/nio/Buffer;){seg}"));
    assert_eq!(
        filter_args_adapter_descriptor(&format!("({seg})J"), &[ofbuf], 0).as_deref(),
        Some("(Ljava/nio/Buffer;)J")
    );
    // A null filter keeps its parameter; `pos` offsets the replacement;
    // a primitive-changing filter changes the slot's type too.
    let tdesc = "(Ljava/lang/String;JLjava/lang/Object;)V";
    assert_eq!(
        filter_args_adapter_descriptor(tdesc, &[None, Some("(I)Ljava/lang/Object;".into())], 1)
            .as_deref(),
        Some("(Ljava/lang/String;JI)V")
    );
    assert_eq!(
        filter_args_adapter_descriptor(tdesc, &[Some("(I)J".into())], 1).as_deref(),
        Some("(Ljava/lang/String;ILjava/lang/Object;)V")
    );
    // Too many filters, or a non-unary filter: unknown, caller keeps target.
    assert_eq!(
        filter_args_adapter_descriptor(tdesc, &[None, None], 2),
        None
    );
    assert_eq!(
        filter_args_adapter_descriptor(tdesc, &[Some("(II)J".into())], 1),
        None
    );
}

// -----------------------------------------------------------------------
// F29 — the collector element's WRAPPER CLASS
//
// `collector_element_box_desc` is a pure function of (component
// descriptor, `Value` variant), so it is tested as one: no mock, no VM, no
// slot table. Every row below is a MEASURED HotSpot 25.0.3+9 observable
// from `CollBox.java` / `CollBox2.java`, named in the assertion message so
// a future reader can re-run the row rather than re-derive it.
// -----------------------------------------------------------------------

/// The generic-entry flag is ONE-SHOT, and that is the whole reason an
/// adapter's inner target keeps its exact-type semantics.
///
/// `invokeWithArguments` arms it; the first [`mh_dispatch`] takes it; every
/// recursive dispatch an adapter arm makes below that sees `false`. If
/// `take_generic_spread` ever became a peek, a null reaching an inner
/// collector through e.g. `filterArguments(collector, 0, f)` would be
/// collected where HotSpot passes it straight through.
/// The passthrough shortcut's condition, row for row against
/// `probes/MhVarargsNullProbe.java`'s G-block.
#[test]
fn the_call_site_decides_whether_a_collector_wraps() {
    let vf = "([Ljava/lang/String;)Ljava/lang/String;";
    let ov = "([Ljava/lang/Object;)Ljava/lang/String;";
    let mx = "(Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/String;";
    let iv = "([I)Ljava/lang/String;";
    // G01/G06/G09 — the call site names the array type exactly.
    assert!(call_site_names_trailing_array(Some(vf), vf));
    assert!(call_site_names_trailing_array(
        Some("(Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/String;"),
        mx
    ));
    assert!(call_site_names_trailing_array(Some(iv), iv));
    // G03/G04 — `Object[]` IS assignable from `String[]`, so still a
    // passthrough. An exact-match-only rule would have collected here.
    assert!(call_site_names_trailing_array(Some(vf), ov));
    // G02/G05/G07 — an `Object` call site never names an array.
    assert!(!call_site_names_trailing_array(
        Some("(Ljava/lang/Object;)Ljava/lang/String;"),
        vf
    ));
    assert!(!call_site_names_trailing_array(
        Some("(Ljava/lang/String;Ljava/lang/Object;)Ljava/lang/String;"),
        mx
    ));
    // The generic door, whose trailing parameter is always `Object`.
    assert!(!call_site_names_trailing_array(
        Some(&generic_method_type_descriptor(1)),
        vf
    ));
    // A differing arity is not a passthrough at all — the collector has to
    // gather, which is what makes `iwa()` an empty array rather than a
    // refusal.
    assert!(!call_site_names_trailing_array(
        Some(&generic_method_type_descriptor(3)),
        vf
    ));
    // `String[]` does NOT accept an `Object[]` call site (the unsound
    // direction), and no call site at all is never a passthrough.
    assert!(!call_site_names_trailing_array(Some(ov), vf));
    assert!(!call_site_names_trailing_array(None, vf));
}

#[test]
fn the_generic_method_type_is_all_objects() {
    assert_eq!(generic_method_type_descriptor(0), "()Ljava/lang/Object;");
    assert_eq!(
        generic_method_type_descriptor(2),
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
    );
}

/// A virtual call site names the receiver and `MH_DESC` does not.
#[test]
fn the_receiver_is_stripped_from_a_virtual_call_site() {
    assert_eq!(
        descriptor_without_first_param("(LFoo;Ljava/lang/String;)V").as_deref(),
        Some("(Ljava/lang/String;)V")
    );
    assert_eq!(
        descriptor_without_first_param("(LFoo;)Ljava/lang/Object;").as_deref(),
        Some("()Ljava/lang/Object;")
    );
    // Nothing to strip.
    assert_eq!(descriptor_without_first_param("()V"), None);
}

/// The spelling a `ClassCastException` uses. MEASURED rows B06/G02 and H03
/// of `probes/MhVarargsNullProbe.java`.
#[test]
fn the_cast_message_spells_an_array_as_a_descriptor() {
    assert_eq!(jvm_type_display("Ljava/lang/String;"), "java.lang.String");
    assert_eq!(
        jvm_type_display("[Ljava/lang/String;"),
        "[Ljava.lang.String;"
    );
    assert_eq!(jvm_type_display("[I"), "[I");
}

#[test]
fn the_generic_spread_flag_is_taken_not_peeked() {
    // Starts clear on a fresh thread.
    assert_eq!(take_entry_call_site(), None);
    arm_entry_call_site("(Ljava/lang/Object;)Ljava/lang/Object;");
    assert_eq!(
        take_entry_call_site().as_deref(),
        Some("(Ljava/lang/Object;)Ljava/lang/Object;"),
        "the outermost dispatch sees it"
    );
    assert_eq!(take_entry_call_site(), None, "a nested dispatch must not");
    // Arming twice is still one dispatch's worth, and the last one wins.
    arm_entry_call_site("()V");
    arm_entry_call_site("(I)V");
    assert_eq!(take_entry_call_site().as_deref(), Some("(I)V"));
    assert_eq!(take_entry_call_site(), None);
}

#[test]
fn a_wrapper_typed_component_settles_the_element_class() {
    // MEASURED: G.CharacterComp.fromChar = java.lang.Character
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Character;", Value::Int(97)),
        Some("C")
    );
    // MEASURED: G.BooleanComp.fromBool / ByteComp / ShortComp / IntegerComp
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Boolean;", Value::Int(1)),
        Some("Z")
    );
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Byte;", Value::Int(3)),
        Some("B")
    );
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Short;", Value::Int(9)),
        Some("S")
    );
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Integer;", Value::Int(7)),
        Some("I")
    );
    // MEASURED: G.LongComp.fromLong = java.lang.Long, G.LongComp.id = true
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Long;", Value::Long(5)),
        Some("J")
    );
    // MEASURED: G.FloatComp.fromFloat = java.lang.Float, and
    // G.FloatComp.id = FALSE — the class is settled, the identity is not.
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Float;", Value::Float(1.5)),
        Some("F")
    );
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Double;", Value::Double(1.5)),
        Some("D")
    );
}

/// The negative control, and it is the important half: an `Object[]`
/// collector must NOT be answered from the component, because HotSpot
/// answers it from the CALL SITE and this arm cannot see one.
/// MEASURED: `coll.type()` = `(Object)Object` while `coll.invoke(aChar)`
/// is a `Character` and `coll.invoke(anInt)` an `Integer`.
#[test]
fn a_non_wrapper_component_settles_nothing() {
    for comp in [
        "Ljava/lang/Object;",
        "Ljava/lang/Number;",
        "Ljava/lang/Comparable;",
        "Ljava/lang/String;",
        "Ljava/io/Serializable;",
        "[I",
        "",
    ] {
        assert_eq!(
            collector_element_box_desc(comp, Value::Int(97)),
            None,
            "{comp} must fall back to the Value variant, not invent a wrapper"
        );
    }
    // MEASURED: C.componentNumber.fromInt = java.lang.Integer, i.e. the
    // fallback's answer is already right for `Number[]` from an `int`;
    // C.componentNumber.fromChar THROWS on HotSpot, so there is no row
    // this `None` gets wrong.
}

/// The variant guard. `("Ljava/lang/Long;", Value::Int(5))` routed on the
/// descriptor alone reaches `native_long_value_of`, which reads
/// `Some(Value::Long(v))` and defaults to **0** — an identity fix turned
/// into a wrong answer. HotSpot never accepts that pair either
/// (MEASURED: `G.LongComp.fromInt` throws `WrongMethodTypeException`), so
/// declining costs nothing.
#[test]
fn the_component_is_not_trusted_against_a_mismatched_value_variant() {
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Long;", Value::Int(5)),
        None
    );
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Integer;", Value::Long(7)),
        None
    );
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Character;", Value::Long(97)),
        None
    );
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Float;", Value::Double(1.5)),
        None
    );
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Double;", Value::Float(1.5)),
        None
    );
    // A reference element travelling through a wrapper-typed collector —
    // already boxed by the caller — must be left alone, not re-boxed.
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Character;", Value::Object(None)),
        None
    );
}

/// `char` and `int` are the same `Value::Int` and must still separate on
/// the component. This is the single row that fails if a later edit
/// "simplifies" the helper back to a variant-only match — which is exactly
/// the state this lane found.
#[test]
fn char_and_int_separate_on_the_component_though_the_variant_cannot() {
    let same_bits = Value::Int(97);
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Character;", same_bits),
        Some("C")
    );
    assert_eq!(
        collector_element_box_desc("Ljava/lang/Integer;", same_bits),
        Some("I")
    );
    assert_ne!(
        collector_element_box_desc("Ljava/lang/Character;", same_bits),
        collector_element_box_desc("Ljava/lang/Integer;", same_bits)
    );
}

// -----------------------------------------------------------------------
// F29-1 NOMINATION 2, closed — the CALL SITE settles an `Object[]`
// collector's wrapper class once the component cannot.
//
// MEASURED (`CollBox.java`): `coll.type()` is `(Object)Object` for every
// one of the six rows below, so nothing about the collector or the
// target's `MethodType` can separate them — only the descriptor the
// caller actually wrote at the call site does.

#[test]
fn the_call_site_settles_a_generic_collectors_wrapper_class() {
    // `coll.invoke(aChar)` — call site `(C)Ljava/lang/Object;`.
    assert_eq!(
        collector_call_site_prim(Some("(C)Ljava/lang/Object;"), 0),
        Some("C")
    );
    // `coll.invoke(aBoolean)`.
    assert_eq!(
        collector_call_site_prim(Some("(Z)Ljava/lang/Object;"), 0),
        Some("Z")
    );
    // `coll.invoke(aByte)` / `coll.invoke(aShort)` / `coll.invoke(anInt)`.
    assert_eq!(
        collector_call_site_prim(Some("(B)Ljava/lang/Object;"), 0),
        Some("B")
    );
    assert_eq!(
        collector_call_site_prim(Some("(S)Ljava/lang/Object;"), 0),
        Some("S")
    );
    assert_eq!(
        collector_call_site_prim(Some("(I)Ljava/lang/Object;"), 0),
        Some("I")
    );
}

/// No armed call site (a miss on `poly_call_site`, or a dispatch door that
/// never arms one — `invokeWithArguments`, an adapter re-entry) must fall
/// back to `None`, exactly the pre-NOMINATION-2 behaviour, never a wrong
/// guess.
#[test]
fn a_missing_call_site_settles_nothing() {
    assert_eq!(collector_call_site_prim(None, 0), None);
    // Out of range: fewer call-site params than the index asks for.
    assert_eq!(
        collector_call_site_prim(Some("()Ljava/lang/Object;"), 0),
        None
    );
    // A reference parameter at the call site carries no primitive either.
    assert_eq!(
        collector_call_site_prim(Some("(Ljava/lang/String;)Ljava/lang/Object;"), 0),
        None
    );
}

/// Multiple leading (non-collected) parameters: the index into the call
/// site must be the ARGUMENT's own position, not always 0.
#[test]
fn the_call_site_index_tracks_the_collected_arguments_position() {
    assert_eq!(
        collector_call_site_prim(Some("(ILjava/lang/Object;C)Ljava/lang/Object;"), 0),
        Some("I")
    );
    assert_eq!(
        collector_call_site_prim(Some("(ILjava/lang/Object;C)Ljava/lang/Object;"), 2),
        Some("C")
    );
    assert_eq!(
        collector_call_site_prim(Some("(ILjava/lang/Object;C)Ljava/lang/Object;"), 1),
        None
    );
}

/// `box_collector_element`'s three-way priority, end to end: component
/// first, call site second, `Value` variant last. `char`/`int` share a
/// `Value` variant and must still separate once either of the first two
/// is available — this is the same defect shape as
/// `char_and_int_separate_on_the_component_though_the_variant_cannot`,
/// one door over.
#[test]
fn box_collector_element_prefers_component_then_call_site_then_variant() {
    let mut ctx = MockNativeContext::new();
    // Its own VM identity: the `Integer` / `Character` box caches are keyed
    // by it, and a box another test's mock cached under the shared default
    // `0` names a class this mock never registered.
    ctx.set_vm_identity(0x5EED_B0C5);
    let same_bits = Value::Int(97);

    // Component settles it: call-site value must not override a
    // wrapper-typed array (`Character[]` still wins over an `int` call
    // site, which cannot arise on HotSpot anyway but must not corrupt the
    // component's own answer if it did).
    let boxed = box_collector_element(&mut ctx, same_bits, "Ljava/lang/Character;", Some("I"));
    assert_eq!(
        ctx.class_name_of_id(ctx.class_id_of_object(match boxed {
            Value::Object(Some(o)) => o,
            _ => panic!("expected a boxed object"),
        })),
        Some("java/lang/Character".to_string())
    );

    // No component (`Object[]`), call site says `char`.
    let boxed = box_collector_element(&mut ctx, same_bits, "Ljava/lang/Object;", Some("C"));
    assert_eq!(
        ctx.class_name_of_id(ctx.class_id_of_object(match boxed {
            Value::Object(Some(o)) => o,
            _ => panic!("expected a boxed object"),
        })),
        Some("java/lang/Character".to_string())
    );

    // No component, no call site: falls back to the `Value` variant,
    // exactly the pre-NOMINATION-2 answer.
    let boxed = box_collector_element(&mut ctx, same_bits, "Ljava/lang/Object;", None);
    assert_eq!(
        ctx.class_name_of_id(ctx.class_id_of_object(match boxed {
            Value::Object(Some(o)) => o,
            _ => panic!("expected a boxed object"),
        })),
        Some("java/lang/Integer".to_string())
    );
}

// -----------------------------------------------------------------------
// F39 — the VarHandle field path's widener
//
// MEASURED on OpenJDK 25.0.3+9 (`scratchpad/f29/VhLong.java`, identical
// under `-Xint`):
//
//   vh.fieldLong     = java.lang.Long,   value 5,   id true
//   vh.getAndSetLong = java.lang.Long,   value 5,   id true
//   vh.fieldDouble   = java.lang.Double, value 1.5, id FALSE
//
// so the direction is `Field.get`'s: widen FIRST, box canonically second.
// Both tests below are about the FUNCTION CHOICE, which is the half of
// this fix that a behavioural test cannot see — every wrong answer here is
// still a `Double` of the right class, and `vh.fieldDouble` is not even an
// identity row.
// -----------------------------------------------------------------------

/// The two wideners are NOT interchangeable, and that is the whole reason
/// the four arms reach into `lang_class` rather than call the local one
/// already in scope. Pure functions on both sides — no mock, no VM, no
/// slot table, so nothing here can measure the mock instead of the rule.
#[test]
fn the_field_widener_reinterprets_where_the_local_one_converts() {
    // The raw slot of a `double` field holding 1.5, exactly as
    // `ctx.get_field` hands it back: the IEEE-754 bit pattern, carried in
    // a `Value::Long`. Decimal, for a reader checking by hand:
    // 4_609_434_218_613_702_656.
    let bits = Value::Long(1.5f64.to_bits() as i64);

    // MEASURED: vh.fieldDouble.value = 1.5. The `1.5` here is a literal,
    // not a restatement of the implementation — this assertion is the one
    // that fails if the field path is ever "unified" onto the numeric
    // widener.
    assert_eq!(
        crate::lang_class::coerce_reflective_field_value(bits, DESC_DOUBLE),
        Value::Double(1.5),
        "the field path must REINTERPRET the slot's bits, not convert them"
    );
    // The local one converts NUMERICALLY, which is correct where it is
    // used (a collector element really is a number being widened) and
    // catastrophic on a field read: 1.5 comes back as ~4.609e18.
    assert_eq!(
        widen_primitive_to_descriptor(bits, DESC_DOUBLE),
        Value::Double(1.5f64.to_bits() as i64 as f64)
    );
    // Same input, same descriptor, two different answers. Anyone
    // collapsing the two functions has to delete this line to do it.
    assert_ne!(
        crate::lang_class::coerce_reflective_field_value(bits, DESC_DOUBLE),
        widen_primitive_to_descriptor(bits, DESC_DOUBLE)
    );
    // The `J` arm — a `long` field holding 5 whose slot presents as a
    // compact `Value::Int` — is where the two AGREE. It is included so
    // the `D` row above cannot be read as "the two functions differ
    // everywhere", which would make the choice look arbitrary rather than
    // forced.
    assert_eq!(
        crate::lang_class::coerce_reflective_field_value(Value::Int(5), DESC_LONG),
        Value::Long(5)
    );
    assert_eq!(
        widen_primitive_to_descriptor(Value::Int(5), DESC_LONG),
        Value::Long(5)
    );
}

/// A SOURCE WITNESS for the call sites, because the set is **four** and
/// three of them look like the whole set — F19-1 N3 scoped this fix to
/// `varhandle_get`'s three field arms, and `vh_box_access_result` (the RMW
/// funnel, measured by `vh.getAndSetLong`) is the fourth. No behavioural
/// test in this module can stand in: `MockNativeContext`'s slots are not
/// the VM's, and the test above proves the FUNCTION is right whether or
/// not anything calls it.
///
/// Needles are assembled with `format!` at runtime: spelled as literals
/// they would match this test's own source text, since the file being
/// searched IS this file. Whitespace and optional trailing commas are
/// stripped so a rustfmt re-wrap cannot break them.
#[test]
fn all_four_varhandle_read_arms_widen_before_boxing() {
    let src = LANG_INVOKE_SRC;
    let squashed: String = src.chars().filter(|c| !c.is_whitespace()).collect();
    let squashed = squashed.replace(",)", ")");
    let widen = format!("crate::lang_class::coerce_reflective_field_{}", "value");
    for (needle, why) in [
        (
            format!("slot0_read_keep_alive(ctx,receiver,slot,raw);letval={widen}(raw,&td);"),
            "varhandle_get's VH_KIND_INSTANCE by-index arm no longer \
             widens, so a `long` field holding 5 boxes to a `Long` \
             carrying compact-Int bits (measured: vh.fieldLong.value = 5)",
        ),
        (
            format!("slot0_read_keep_alive(ctx,receiver,idx,raw);letval={widen}(raw,&td);"),
            "varhandle_get's by-NAME arm no longer widens — the same read \
             after a late resolve, which must not answer differently from \
             one that resolved early",
        ),
        (
            format!("letval={widen}(raw,&td);"),
            "VH_KIND_STATIC no longer widens; `get_static_field` is as raw \
             as `get_field` and this arm is not a bystander",
        ),
        (
            format!("letvalue={widen}(value,&desc);"),
            "vh_box_access_result — the RMW funnel — no longer widens. \
             This is the arm F19-1 N3 missed: measured, vh.getAndSetLong \
             hands back the old value 5 as a canonical Long, so the same \
             raw slot reaches this funnel through getAndSet",
        ),
    ] {
        assert!(squashed.contains(&needle), "{why} (`{needle}` is gone)");
    }
}

// MH_KIND_DROP dispatch must trim the dynamic args using the EXACT
// `pos:count` encoded at construction time (see MH_KIND_RETURN_FILTER's
// sibling doc comment on `make_drop_arguments_adapter` for the full
// story) rather than re-deriving the drop count later from
// `extra_args.len() - inner_expected`. Regression coverage for the
// JRubyScriptTemplateTests investigation (2026-07-15): a bare
// `dropArguments(leaf, pos, valueTypes)` adapter, dispatched with the
// widened arg list, must forward ONLY the kept (non-dropped) argument
// to `leaf`, regardless of what values sit in the dropped slots.
#[test]
fn drop_arguments_dispatch_keeps_correct_slot_not_adjacent_ones() {
    let mut ctx = MockNativeContext::new();
    // Leaf: identity(x) = x -- a plain 1-arg handle, so the dispatch
    // result directly tells us which argument survived the drop.
    let leaf = alloc_method_handle(
        &mut ctx,
        "java/lang/invoke/MethodHandles",
        "identity",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        MH_KIND_IDENTITY,
    )
    .unwrap();
    // Simulates `dropArguments(leaf, 1, [Object.class, Object.class])`:
    // a 3-param adapter where params[1..3] are dropped and param[0] is
    // the one forwarded to `leaf`. Built directly (bypassing
    // `make_drop_arguments_adapter`'s `[Ljava/lang/Class;` machinery)
    // to isolate the dispatch-side fix under test.
    let adapter = alloc_method_handle(
        &mut ctx,
        "1:2",
        "drop",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        MH_KIND_DROP,
    )
    .unwrap();
    ctx.set_field(adapter, MH_BOUND, Value::Object(Some(leaf)));

    // Three distinct sentinel objects so a wrong-position bug (e.g. a
    // dropped arg silently reaching `leaf` instead of the kept one)
    // is unmistakable rather than accidentally passing.
    let kept = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 0).unwrap();
    let dropped1 = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 0).unwrap();
    let dropped2 = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 0).unwrap();
    let args = [
        Value::Object(Some(kept)),
        Value::Object(Some(dropped1)),
        Value::Object(Some(dropped2)),
    ];
    let result = mh_dispatch(&mut ctx, adapter, &args).unwrap();
    assert_eq!(
        result,
        Some(Value::Object(Some(kept))),
        "dropArguments(leaf, pos=1, count=2) must keep only the arg at \
         pos 0 and forward it to leaf, regardless of the dropped args"
    );
}

// C13: alloc_method_handle must populate the real-JDK
// MethodHandle.type:MethodType field so JDK code paths that read
// `mh.type()` see a non-null MethodType (not NPE / CCE).
#[test]
fn alloc_method_handle_populates_type_field() {
    let mut ctx = MockNativeContext::new();
    let mh = alloc_method_handle(
        &mut ctx,
        "java/lang/Integer",
        "parseInt",
        "(Ljava/lang/String;)I",
        MH_KIND_STATIC,
    )
    .unwrap();
    // In the mock, `set_field_by_name("type", ...)` maps to slot 2
    // (see test_utils::mock_jdk_field_slot) — that's the same slot
    // the MH write-path targets, so reading it back yields the
    // MethodType that was installed. The key assertion is simply
    // that the value is a non-null object (a real MethodType).
    let t = ctx.get_field_by_name(mh, "type");
    match t {
        Value::Object(Some(_)) => {}
        other => panic!(
            "expected non-null MethodType at 'type' field, got {:?}",
            other
        ),
    }
}

/// Interpreter round i1 wave 20, lane L3: a handle minted by
/// `alloc_method_handle` carries `MH_STATIC_SETTLED`, unsettled until a
/// settled owner is recorded, without disturbing the other slots.
#[test]
fn a_minted_handle_starts_unsettled_and_remembers_its_settled_owner() {
    let mut ctx = MockNativeContext::new();
    let mh = alloc_method_handle(
        &mut ctx,
        "java/lang/Integer",
        "parseInt",
        "(Ljava/lang/String;)I",
        MH_KIND_STATIC,
    )
    .unwrap();
    assert!(mh_has_static_settled_slot(&ctx, mh));
    assert_eq!(mh_static_settled(&ctx, mh), None);
    mh_set_static_settled(&ctx, mh, ClassId::new(41));
    assert_eq!(mh_static_settled(&ctx, mh), Some(ClassId::new(41)));
    // The other synthetic slots are untouched by the new one.
    assert!(!mh_is_varargs_collector(&ctx, mh));
    assert!(matches!(
        ctx.get_field(mh, MH_KIND),
        Value::Int(MH_KIND_STATIC)
    ));
}

/// Interpreter round i2 wave 66, lane L3: the carrier's door-key line
/// hands the same slot back for the same VM, epoch, carrier and member
/// objects, and a fresh empty slot when any of them differs (a
/// collection, an in-place member rewrite, another VM, another carrier
/// in the same line).
#[test]
fn the_carrier_door_key_line_keeps_one_slot_per_carrier_and_epoch() {
    use cratonvm_native_api::SettledDoorKeyEntry;
    let vm = 0x5150usize;
    let carrier = 0x7000usize;
    let refs = [0x100usize, 0x200, 0x300];
    let first = carrier_door_key_line(vm, 9, carrier, refs);
    assert_eq!(first.get(), None);
    first.set(Some(SettledDoorKeyEntry {
        owner: ClassId::new(12),
        redefinitions: 0,
        key: Some((ClassId::new(12), 3)),
    }));
    let again = carrier_door_key_line(vm, 9, carrier, refs);
    assert!(std::rc::Rc::ptr_eq(&first, &again));
    assert_eq!(
        again.lookup(ClassId::new(12), 0),
        Some(Some((ClassId::new(12), 3)))
    );
    // A collection moved the epoch: a fresh slot.
    let moved = carrier_door_key_line(vm, 10, carrier, refs);
    assert!(!std::rc::Rc::ptr_eq(&first, &moved));
    assert_eq!(moved.get(), None);
    // A rewritten descriptor object in the same epoch: a fresh slot.
    let rewritten = carrier_door_key_line(vm, 10, carrier, [0x100, 0x200, 0x308]);
    assert!(!std::rc::Rc::ptr_eq(&moved, &rewritten));
    assert_eq!(rewritten.get(), None);
    // Another VM: a fresh slot.
    let other_vm = carrier_door_key_line(vm + 1, 10, carrier, [0x100, 0x200, 0x308]);
    assert!(!std::rc::Rc::ptr_eq(&rewritten, &other_vm));
    // Another carrier mapping to the same line evicts it.
    let colliding = carrier + (CARRIER_DOOR_KEY_MEMO_SLOTS << 3);
    let evictor = carrier_door_key_line(vm + 1, 10, colliding, [0x100, 0x200, 0x308]);
    assert!(!std::rc::Rc::ptr_eq(&other_vm, &evictor));
    let back = carrier_door_key_line(vm + 1, 10, carrier, [0x100, 0x200, 0x308]);
    assert!(!std::rc::Rc::ptr_eq(&other_vm, &back));
    assert_eq!(back.get(), None);
}

/// Interpreter round i2 wave 66, lane L3: with the switch off (the
/// trait's default, the mock's answer) a settled carrier keeps no door
/// key, and an unsettled one never asks.
#[test]
fn a_carrier_keeps_no_door_key_with_the_switch_off() {
    let mut ctx = MockNativeContext::new();
    let mh = alloc_method_handle(
        &mut ctx,
        "java/lang/Integer",
        "parseInt",
        "(Ljava/lang/String;)I",
        MH_KIND_STATIC,
    )
    .unwrap();
    assert!(mh_carrier_door_key(&ctx, mh, None).is_none());
    assert!(mh_carrier_door_key(&ctx, mh, Some(ClassId::new(41))).is_none());
}

#[test]
fn serialization_hook_neutral_result_matches_objectstream_hooks() {
    assert_eq!(
        serialization_hook_neutral_result("readObject", "(Ljava/io/ObjectInputStream;)V", &[]),
        Some(None)
    );
    assert_eq!(
        serialization_hook_neutral_result(
            "readResolve",
            "()Ljava/lang/Object;",
            &[Value::Object(None)]
        ),
        Some(Some(Value::Object(None)))
    );
    assert_eq!(
        serialization_hook_neutral_result("clone", "()Ljava/lang/Object;", &[]),
        None
    );
}

// Sanity check: build_method_type_from_descriptor turns a plain
// descriptor string into a non-null 2-field MethodType object.
#[test]
fn build_method_type_from_descriptor_non_null() {
    let mut ctx = MockNativeContext::new();
    let mt = build_method_type_from_descriptor(&mut ctx, "(Ljava/lang/String;)I").unwrap();
    assert!(
        mt.is_some(),
        "MethodType should be non-null for a valid descriptor"
    );
}

// Invalid descriptors should return None rather than panicking.
#[test]
fn build_method_type_from_descriptor_rejects_garbage() {
    let mut ctx = MockNativeContext::new();
    assert!(build_method_type_from_descriptor(&mut ctx, "")
        .unwrap()
        .is_none());
    assert!(
        build_method_type_from_descriptor(&mut ctx, "not-a-descriptor")
            .unwrap()
            .is_none()
    );
}

#[test]
fn direct_primitive_return_boxes_boolean_for_adapter_chains() {
    let mut ctx = MockNativeContext::new();
    let mh = alloc_method_handle(
        &mut ctx,
        "java/util/Map",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        MH_KIND_VIRTUAL,
    )
    .unwrap();
    let effective_desc =
        ctx.create_string("(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;");
    ctx.set_field(mh, MH_DESC, Value::Object(Some(effective_desc)));
    // The effective type is the handle's `type`, which the mock now reads
    // (`mock_method_type_field_slot`); `MH_DESC` is only its fallback.
    // Stamp it as an adapter chain's `asType` copy carries it.
    let effective_type = build_method_type_from_descriptor(
        &mut ctx,
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    )
    .unwrap()
    .expect("MethodType");
    ctx.set_field_by_name(mh, "type", Value::Object(Some(effective_type)));
    let result = box_direct_primitive_return(
        &mut ctx,
        mh,
        Ok(Some(Value::Int(1))),
        "(Ljava/lang/Object;)Z",
    )
    .expect("direct return should not throw")
    .expect("direct return should produce a value");
    match result {
        Value::Object(Some(obj)) => {
            // The wrapper class-id cache is process-global while each
            // MockNativeContext has a private class table, so class-name
            // lookup can be stale here. The payload proves boxing happened.
            assert_eq!(ctx.get_field(obj, 0), Value::Int(1));
        }
        other => panic!("expected boxed Boolean, got {:?}", other),
    }
}

#[test]
fn direct_primitive_return_keeps_raw_for_primitive_effective_type() {
    let mut ctx = MockNativeContext::new();
    let mh = alloc_method_handle(
        &mut ctx,
        "java/util/Map",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        MH_KIND_VIRTUAL,
    )
    .unwrap();
    let result = box_direct_primitive_return(
        &mut ctx,
        mh,
        Ok(Some(Value::Int(1))),
        "(Ljava/lang/Object;)Z",
    )
    .expect("direct return should not throw")
    .expect("direct return should produce a value");
    assert_eq!(result, Value::Int(1));
}

#[test]
fn auto_box_return_preserves_already_boxed_primitive_result() {
    let mut ctx = MockNativeContext::new();
    // The FRESH helper on purpose, and spelled in full because this file
    // no longer imports it. `box_value_canonical("Z", 0)` would hand back
    // the shared `Boolean.FALSE` instance, which is process-global and
    // keyed by `vm_identity()` — a fixture this test then writes slot 0 of
    // would be mutating an object other tests in this binary also hold.
    let boxed = match crate::lang_class::box_value(&mut ctx, Value::Int(0), "Z") {
        Value::Object(Some(obj)) => obj,
        other => panic!("expected boxed Boolean fixture, got {:?}", other),
    };
    let result = auto_box_return(&mut ctx, Ok(Some(Value::Object(Some(boxed)))), "()Z")
        .expect("auto box should not throw")
        .expect("auto box should produce a value");
    assert_eq!(result, Value::Object(Some(boxed)));
    assert_eq!(ctx.get_field(boxed, 0), Value::Int(0));
}

#[test]
fn guard_truthiness_unboxes_boxed_boolean_false() {
    let mut ctx = MockNativeContext::new();
    let boxed_false = match ctx.new_object("java/lang/Boolean").unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected boxed Boolean fixture, got {:?}", other),
    };
    ctx.set_field(boxed_false, 0, Value::Int(0));
    let boxed_true = match ctx.new_object("java/lang/Boolean").unwrap() {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("expected boxed Boolean fixture, got {:?}", other),
    };
    ctx.set_field(boxed_true, 0, Value::Int(1));
    assert!(!mh_guard_truthy(
        &ctx,
        Some(Value::Object(Some(boxed_false)))
    ));
    assert!(mh_guard_truthy(&ctx, Some(Value::Object(Some(boxed_true)))));
}

// C33: alloc_resolved_member_name must produce a MemberName that reads
// as fully resolved (resolution == null) with a non-zero vmindex sentinel,
// a non-null name/type/clazz, and a non-zero flags field.
//
// Read through the same accessors the production code writes through
// (`mn_get`, `mn_get_vmindex`). The earlier version of this test read
// `clazz`/`name`/`type`/`flags` with `get_field_by_name` and the vmindex
// with a raw `get_field(mn, 4)` — a split that only agreed because
// `MockNativeContext` keeps by-name and by-index writes in two separate
// maps and the production code wrote BOTH. `MockNativeContext` declares no
// fields and resolves no field names, so `mn_*` here takes the fabricated-
// layout branch, which is the layout this allocation path produces when the
// real `MemberName` is absent. The real-layout branch is not mock-testable
// (it needs a loaded class); it is measured by the overlay census instead —
// 7 `Int`-over-`L` writes at slot 4 per `JdkOnlyBreadthProbe` run before the
// fix, 0 after.
#[test]
fn c33_alloc_resolved_member_name_populates_all_slots() {
    let mut ctx = MockNativeContext::new();
    let mn = alloc_resolved_member_name(
        &mut ctx,
        "java/lang/invoke/LambdaForm",
        "interpret_V",
        "()V",
    )
    .unwrap();
    match mn_get(&mut ctx, mn, "clazz", MN_CLAZZ) {
        Value::Object(Some(_)) => {}
        other => panic!("expected clazz to be non-null, got {:?}", other),
    }
    match mn_get(&mut ctx, mn, "name", MN_NAME) {
        Value::Object(Some(_)) => {}
        other => panic!("expected name to be non-null, got {:?}", other),
    }
    match mn_get(&mut ctx, mn, "type", MN_TYPE) {
        Value::Object(Some(_)) => {}
        other => panic!("expected type to be non-null, got {:?}", other),
    }
    match mn_get(&mut ctx, mn, "flags", MN_FLAGS) {
        Value::Int(f) => {
            assert!(f != 0, "flags must be non-zero");
            assert!((f & 0x1_0000) != 0, "IS_METHOD bit must be set");
            assert!(
                (f >> 24) & 0x0F == 6,
                "refKind must be REF_invokeStatic (6)"
            );
        }
        other => panic!("expected flags to be Int, got {:?}", other),
    }
    assert_ne!(
        mn_get_vmindex(&mut ctx, mn),
        0,
        "vmindex sentinel must be non-zero on our own layout"
    );
    // resolution == null is what the real `MemberName.isResolved()` reads.
    match mn_get(&mut ctx, mn, "resolution", MN_RESOLUTION) {
        Value::Object(None) => {}
        other => panic!("expected null at the resolution slot, got {:?}", other),
    }
}

// The fabricated-layout predicate must answer "ours" for an object whose
// class declares no `method` field, which is what `MockNativeContext` (and
// `ensure_synthetic_class`, whose slots are named `_f0.._fN`) reports. The
// real-layout half is measured by the census; see the note above.
#[test]
fn mn_synthetic_layout_predicate_is_true_without_a_method_field() {
    let mut ctx = MockNativeContext::new();
    let mn =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MemberName", MN_FIELD_COUNT)
            .unwrap();
    assert!(
        mn_has_synthetic_layout(&mut ctx, mn),
        "a class declaring no instance field named `method` is not a real MemberName"
    );
}

// C33: the three IBG native bypass functions must each return a non-null
// MemberName without panicking, for the common entry-point signatures.
#[test]
fn c33_ibg_interpreter_entry_point_returns_non_null() {
    let mut ctx = MockNativeContext::new();
    let mt = build_method_type_from_descriptor(&mut ctx, "(I)J")
        .unwrap()
        .expect("MethodType");
    let result =
        native_ibg_generate_interpreter_entry_point(&mut ctx, &[Value::Object(Some(mt))]);
    match result {
        Ok(Some(Value::Object(Some(_)))) => {}
        other => panic!("expected non-null MemberName, got {:?}", other),
    }
}

#[test]
fn c33_ibg_customized_code_returns_non_null() {
    let mut ctx = MockNativeContext::new();
    // A LambdaForm and MethodType. Synthetic alloc for the form is fine
    // since the native doesn't read its fields.
    let form =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/LambdaForm", 8).unwrap();
    let mt = build_method_type_from_descriptor(&mut ctx, "()V")
        .unwrap()
        .expect("MethodType");
    let result = native_ibg_generate_customized_code(
        &mut ctx,
        &[Value::Object(Some(form)), Value::Object(Some(mt))],
    );
    match result {
        Ok(Some(Value::Object(Some(_)))) => {}
        other => panic!("expected non-null MemberName, got {:?}", other),
    }
}

#[test]
fn c33_ibg_named_function_invoker_returns_non_null() {
    let mut ctx = MockNativeContext::new();
    let form =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodTypeForm", 6).unwrap();
    let result =
        native_ibg_generate_named_function_invoker(&mut ctx, &[Value::Object(Some(form))]);
    match result {
        Ok(Some(Value::Object(Some(_)))) => {}
        other => panic!("expected non-null MemberName, got {:?}", other),
    }
}

// Regression: `varhandle_get_and_add` (and getAndSet / compareAndExchange)
// must resolve kind + field from the `vh_meta` side table, NOT the synthetic
// VH_KIND slot — real-JDK VarHandles (findVarHandle) carry no synthetic
// layout, so a raw slot read mis-resolves and the update silently no-ops
// returning 0. Here the synthetic VH_KIND slot is deliberately garbage; only
// the meta makes the instance update succeed.
fn vh_with_garbage_kind(ctx: &mut MockNativeContext) -> ObjectRef {
    let vh = match ctx.new_object("java/lang/invoke/VarHandle").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    ctx.set_field(vh, VH_KIND, Value::Int(99)); // not INSTANCE/STATIC/ARRAY
    vh
}

/// A mock with a PRIVATE vm identity, for any test whose subject reaches
/// `box_value`.
///
/// `box_value` allocates through `alloc_wrapper`, whose wrapper-`ClassId`
/// cache is PROCESS-GLOBAL and keyed by `(vm_identity, wrapper)`. Every
/// untouched `MockNativeContext` reports identity `0`, while each one's
/// `ClassId` counter is independent — so two such tests running in parallel
/// share one cache row, and the second would allocate its box under the
/// FIRST test's `ClassId`, making `class_name_of_id` name the wrong class.
/// (Same shape as the parallel-test crash already recorded against these
/// process-global native caches.) A per-test identity makes the row
/// private, so each test resolves the wrapper against its own class table.
fn boxing_ctx() -> MockNativeContext {
    static NEXT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0x5713_0000);
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    ctx
}

/// W8-13: every value-returning `VarHandle` access mode is registered under
/// the erased `…)Ljava/lang/Object;` descriptor and must therefore hand back
/// a BOX, not a bare primitive — see [`vh_box_access_result`]. This asserts
/// both halves at once: the wrapper class the JDK 25 oracle names for the
/// variable's type, and the payload in slot 0.
///
/// Written as an assertion rather than a bare unwrap on purpose: the four
/// tests below previously read `Some(Value::Int(n))` and so PASSED on the
/// unboxed shape that printed `null` at an `Object` call site.
fn expect_boxed(
    ctx: &MockNativeContext,
    result: MethodCallResult,
    wrapper: &str,
    payload: Value,
) {
    let obj = match result {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("expected a boxed {wrapper}, got {other:?}"),
    };
    let cid = ctx.class_id_of_object(obj);
    assert_eq!(
        ctx.class_name_arc_of_id(cid).as_deref(),
        Some(wrapper),
        "boxed with the wrong wrapper class"
    );
    assert_eq!(ctx.get_field(obj, 0), payload, "boxed payload");
}

#[test]
fn get_and_add_resolves_via_meta_not_synthetic_kind() {
    let mut ctx = boxing_ctx();
    let vh = vh_with_garbage_kind(&mut ctx);
    let recv = match ctx.new_object("Counter").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    ctx.set_field(recv, 0, Value::Int(10));
    let cid = ctx.class_id_of_object(recv).as_u32();
    vh_meta_put(
        &mut ctx,
        vh,
        VarHandleMeta {
            kind: VH_KIND_INSTANCE,
            class_name: "Counter".to_string(),
            field_name: "n".to_string(),
            field_desc: "I".to_string(),
            field_index: 0,
            class_id: cid,
            read_only: false,
            exact: None,
            ref_type_class_id: 0,
            holder_class_id: 0,
        },
    );
    let old = varhandle_get_and_add(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(recv)),
            Value::Int(5),
        ],
    );
    // Old value via meta (not 0), boxed per the erased Object return.
    expect_boxed(&ctx, old, "java/lang/Integer", Value::Int(10));
    assert_eq!(
        ctx.get_field(recv, 0),
        Value::Int(15),
        "field must be old + delta"
    );
}

/// t1 wave 9, lane ATOM2 (page
/// `t1-rlatom-vh-getandadd-without-a-row-keeps-the-sub-word-carry-20261026`):
/// a handle with NO side-table row narrows the sum by its own
/// `VH_FIELD_DESC` slot, as `Unsafe.getAndAddByte` stores `(byte) (v + d)`.
/// The row-less arm passed `""` and stored the plain `int` 128.
#[test]
fn row_less_get_and_add_narrows_a_byte_field_by_its_desc_slot() {
    let mut ctx = boxing_ctx();
    let vh = match ctx.new_object("java/lang/invoke/VarHandle").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    ctx.set_field(vh, VH_KIND, Value::Int(VH_KIND_INSTANCE));
    let vh = vh_store_names(&mut ctx, vh, "Counter", "b", "B");
    ctx.set_field(vh, VH_FIELD_INDEX, Value::Int(0));
    let recv = match ctx.new_object("Counter").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    ctx.set_field(recv, 0, Value::Int(127));
    let old = varhandle_get_and_add_raw(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(recv)),
            Value::Int(1),
        ],
        &None,
        "getAndAdd",
    );
    assert_eq!(old.ok().flatten(), Some(Value::Int(127)), "old value");
    assert_eq!(
        ctx.get_field(recv, 0),
        Value::Int(-128),
        "a byte 127 plus 1 holds -128, never 128"
    );
}

/// W8-13, and the reason the boxing CANNOT live at the poly-return
/// boundary in `vm_exec.rs`.
///
/// `Value::Int` carries `boolean`, `byte`, `char`, `short` and `int` alike,
/// so the value's tag does not name its wrapper. Only the native knows the
/// array's element type. Measured on the JDK 25 oracle, HotSpot prints
/// `true` for `System.out.println(booleanVh.getAndSet(arr, 1, false))` —
/// i.e. it boxes a `java.lang.Boolean`. A boundary fix that inferred the
/// wrapper from the `Value` tag would produce `java.lang.Integer` and print
/// `1`. This test is the falsifier for that design.
#[test]
fn get_and_set_on_boolean_array_boxes_boolean_not_integer() {
    let mut ctx = boxing_ctx();
    let vh = vh_with_garbage_kind(&mut ctx);
    let arr = ctx.new_array(ArrayElementType::Boolean, 3);
    ctx.set_array_element(arr, 1, Value::Int(1)); // true
    let old = varhandle_get_and_set(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(arr)),
            Value::Int(1),
            Value::Int(0), // false
        ],
    );
    expect_boxed(&ctx, old, "java/lang/Boolean", Value::Int(1));
    assert_eq!(
        ctx.get_array_element(arr, 1),
        Value::Int(0),
        "element must hold the new value"
    );
}

/// The same argument one type over: a `long[]` element must box to
/// `java.lang.Long`, which is what pins `getAndAdd`'s witness to the
/// element descriptor rather than to the `Value` tag.
#[test]
fn get_and_add_on_long_array_boxes_long() {
    let mut ctx = boxing_ctx();
    let vh = vh_with_garbage_kind(&mut ctx);
    let arr = ctx.new_array(ArrayElementType::Long, 3);
    ctx.set_array_element(arr, 1, Value::Long(7));
    let old = varhandle_get_and_add(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(arr)),
            Value::Int(1),
            Value::Long(5),
        ],
    );
    expect_boxed(&ctx, old, "java/lang/Long", Value::Long(7));
    assert_eq!(
        ctx.get_array_element(arr, 1),
        Value::Long(12),
        "old + delta"
    );
}

/// A REFERENCE-typed variable must pass through the funnel untouched — the
/// idempotence property that lets `varhandle_get`'s own inline `box_value`
/// and this funnel coexist without double-boxing.
#[test]
fn get_and_set_on_reference_array_is_not_reboxed() {
    let mut ctx = boxing_ctx();
    let vh = vh_with_garbage_kind(&mut ctx);
    let arr = ctx.new_array(ArrayElementType::Reference, 3);
    let a = match ctx.new_object("Alpha").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    let b = match ctx.new_object("Beta").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    ctx.set_array_element(arr, 1, Value::Object(Some(a)));
    let old = varhandle_get_and_set(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(arr)),
            Value::Int(1),
            Value::Object(Some(b)),
        ],
    );
    assert_eq!(
        old.unwrap(),
        Some(Value::Object(Some(a))),
        "a reference witness must be returned as-is, never wrapped"
    );
    assert_eq!(ctx.get_array_element(arr, 1), Value::Object(Some(b)));
}

#[test]
fn get_and_set_resolves_via_meta_not_synthetic_kind() {
    let mut ctx = boxing_ctx();
    let vh = vh_with_garbage_kind(&mut ctx);
    let recv = match ctx.new_object("Holder").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    ctx.set_field(recv, 0, Value::Int(20));
    let cid = ctx.class_id_of_object(recv).as_u32();
    vh_meta_put(
        &mut ctx,
        vh,
        VarHandleMeta {
            kind: VH_KIND_INSTANCE,
            class_name: "Holder".to_string(),
            field_name: "v".to_string(),
            field_desc: "I".to_string(),
            field_index: 0,
            class_id: cid,
            read_only: false,
            exact: None,
            ref_type_class_id: 0,
            holder_class_id: 0,
        },
    );
    let old = varhandle_get_and_set(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(recv)),
            Value::Int(99),
        ],
    );
    // Old value via meta (not null), boxed per the erased Object return.
    expect_boxed(&ctx, old, "java/lang/Integer", Value::Int(20));
    assert_eq!(
        ctx.get_field(recv, 0),
        Value::Int(99),
        "field must be the new value"
    );
}

#[test]
fn p67_memory_segment_varhandle_access_mode_type_uses_segment_and_offset_coordinates() {
    let mut ctx = MockNativeContext::new();
    let vh = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/VarHandle", 3).unwrap();
    register_p67_memory_segment_var_handle(
        &mut ctx,
        vh,
        SegmentVhShape {
            width: 4,
            carrier: b'I',
            little_endian: true,
            base_offset: 0,
            stride: 0,
            alignment: 4,
            target_size: 0,
            target_align: 1,
            index_bound: None,
            scaled_index: false,
            enclosing_size: 0,
        },
    );

    let access_type =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/VarHandle$AccessType", 2)
            .unwrap();
    ctx.set_field(access_type, 1, Value::Int(0));
    let get_mt = match varhandle_access_mode_type_uncached(
        &mut ctx,
        &[Value::Object(Some(vh)), Value::Object(Some(access_type))],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(mt)) => mt,
        other => panic!("expected MethodType, got {other:?}"),
    };
    assert_eq!(
        descriptor_from_method_type(&ctx, get_mt),
        "(Ljava/lang/foreign/MemorySegment;J)I"
    );

    ctx.set_field(access_type, 1, Value::Int(1));
    let set_mt = match varhandle_access_mode_type_uncached(
        &mut ctx,
        &[Value::Object(Some(vh)), Value::Object(Some(access_type))],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(mt)) => mt,
        other => panic!("expected MethodType, got {other:?}"),
    };
    assert_eq!(
        descriptor_from_method_type(&ctx, set_mt),
        "(Ljava/lang/foreign/MemorySegment;JI)V"
    );
}

/// A `sequenceElement()` path adds a `long` INDEX coordinate, and the
/// carrier — not the width — decides the value type: a `JAVA_FLOAT` handle
/// is four bytes wide and types as `float`.
#[test]
fn p67_layout_varhandle_reports_its_index_coordinate_and_carrier() {
    let mut ctx = MockNativeContext::new();
    let vh = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/VarHandle", 3).unwrap();
    register_p67_memory_segment_var_handle(
        &mut ctx,
        vh,
        SegmentVhShape {
            width: 4,
            carrier: b'F',
            little_endian: true,
            base_offset: 0,
            stride: 4,
            alignment: 4,
            target_size: 0,
            target_align: 1,
            index_bound: None,
            scaled_index: false,
            enclosing_size: 0,
        },
    );
    let access_type =
        try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/VarHandle$AccessType", 2)
            .unwrap();
    ctx.set_field(access_type, 1, Value::Int(0));
    let get_mt = match varhandle_access_mode_type_uncached(
        &mut ctx,
        &[Value::Object(Some(vh)), Value::Object(Some(access_type))],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(mt)) => mt,
        other => panic!("expected MethodType, got {other:?}"),
    };
    assert_eq!(
        descriptor_from_method_type(&ctx, get_mt),
        "(Ljava/lang/foreign/MemorySegment;JJ)F"
    );
}

#[test]
fn compare_and_exchange_resolves_via_meta_not_synthetic_kind() {
    let mut ctx = boxing_ctx();
    let vh = vh_with_garbage_kind(&mut ctx);
    let recv = match ctx.new_object("Cell").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    ctx.set_field(recv, 0, Value::Int(30));
    let cid = ctx.class_id_of_object(recv).as_u32();
    vh_meta_put(
        &mut ctx,
        vh,
        VarHandleMeta {
            kind: VH_KIND_INSTANCE,
            class_name: "Cell".to_string(),
            field_name: "x".to_string(),
            field_desc: "I".to_string(),
            field_index: 0,
            class_id: cid,
            read_only: false,
            exact: None,
            ref_type_class_id: 0,
            holder_class_id: 0,
        },
    );
    // Matching expected → updates and returns the witness (old value).
    let witness = varhandle_compare_and_exchange(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(recv)),
            Value::Int(30),
            Value::Int(77),
        ],
    );
    // Witness = old value via meta, boxed per the erased Object return.
    expect_boxed(&ctx, witness, "java/lang/Integer", Value::Int(30));
    assert_eq!(
        ctx.get_field(recv, 0),
        Value::Int(77),
        "field must be updated on match"
    );
}

// LOW-finding fix regression: on a compareAndExchange whose `expected`
// does NOT match the current field value, the atomic CAS must leave the
// field unchanged and return the *witnessed* current value (not silently
// overwrite it). The previous non-atomic get/compare/set already declined
// to write on mismatch, but routing through `compare_and_swap_field`
// exercises the new linearizable path.
#[test]
fn compare_and_exchange_mismatch_does_not_write() {
    let mut ctx = boxing_ctx();
    let vh = vh_with_garbage_kind(&mut ctx);
    let recv = match ctx.new_object("Cell").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    ctx.set_field(recv, 0, Value::Int(30));
    let cid = ctx.class_id_of_object(recv).as_u32();
    vh_meta_put(
        &mut ctx,
        vh,
        VarHandleMeta {
            kind: VH_KIND_INSTANCE,
            class_name: "Cell".to_string(),
            field_name: "x".to_string(),
            field_desc: "I".to_string(),
            field_index: 0,
            class_id: cid,
            read_only: false,
            exact: None,
            ref_type_class_id: 0,
            holder_class_id: 0,
        },
    );
    // expected = 99 (wrong) → no swap; witness is the real current 30.
    let witness = varhandle_compare_and_exchange(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(recv)),
            Value::Int(99),
            Value::Int(77),
        ],
    );
    // Witness = the current value on mismatch, boxed per the erased return.
    expect_boxed(&ctx, witness, "java/lang/Integer", Value::Int(30));
    assert_eq!(
        ctx.get_field(recv, 0),
        Value::Int(30),
        "field must be unchanged on mismatch"
    );
}

// LOW-finding fix regression: `varhandle_compare_and_set` now routes the
// instance path through the linearizable `compare_and_swap_field`. Verify
// both the success (matching expected → swap, return 1) and the failure
// (wrong expected → no swap, return 0) outcomes.
#[test]
fn compare_and_set_instance_atomic_success_and_failure() {
    let mut ctx = MockNativeContext::new();
    let vh = vh_with_garbage_kind(&mut ctx);
    let recv = match ctx.new_object("Slot").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    ctx.set_field(recv, 0, Value::Int(5));
    let cid = ctx.class_id_of_object(recv).as_u32();
    vh_meta_put(
        &mut ctx,
        vh,
        VarHandleMeta {
            kind: VH_KIND_INSTANCE,
            class_name: "Slot".to_string(),
            field_name: "v".to_string(),
            field_desc: "I".to_string(),
            field_index: 0,
            class_id: cid,
            read_only: false,
            exact: None,
            ref_type_class_id: 0,
            holder_class_id: 0,
        },
    );
    // Wrong expected → no swap, returns 0 (false).
    let miss = varhandle_compare_and_set(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(recv)),
            Value::Int(999),
            Value::Int(42),
        ],
    )
    .unwrap();
    assert_eq!(
        miss,
        Some(Value::Int(0)),
        "mismatch must report CAS failure"
    );
    assert_eq!(
        ctx.get_field(recv, 0),
        Value::Int(5),
        "field must be unchanged on mismatch"
    );
    // Correct expected → swap, returns 1 (true).
    let hit = varhandle_compare_and_set(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(recv)),
            Value::Int(5),
            Value::Int(42),
        ],
    )
    .unwrap();
    assert_eq!(hit, Some(Value::Int(1)), "match must report CAS success");
    assert_eq!(
        ctx.get_field(recv, 0),
        Value::Int(42),
        "field must hold new value on success"
    );
}

/// r11w3-rt-varhandle-array-typed-field-stores-unchecked: an ARRAY-typed
/// field used to admit any object (`reference_arg_admitted` answers "cannot
/// tell" for every `[` descriptor), so `VH.set(h, (Object) "x")` stored a
/// `String` into an `int[]` field where HotSpot throws
/// `ClassCastException`. Refuse what the heap shape settles; a reference array's
/// component is the VM's `checkcast` verdict, `None` (fail open) on this mock.
#[test]
fn access_mode_method_names_are_the_jdks() {
    assert_eq!(access_mode_method_name("GET"), "get");
    assert_eq!(access_mode_method_name("GET_AND_ADD"), "getAndAdd");
    assert_eq!(access_mode_method_name("COMPARE_AND_SET"), "compareAndSet");
    assert_eq!(
        access_mode_method_name("GET_AND_BITWISE_OR_RELEASE"),
        "getAndBitwiseOrRelease"
    );
    assert_eq!(
        access_mode_method_name("WEAK_COMPARE_AND_SET_PLAIN"),
        "weakCompareAndSetPlain"
    );
}

#[test]
fn access_mode_support_follows_the_natives_rules() {
    // Field handles: numeric add, integral/boolean bitwise, final reads only.
    assert!(vh_access_mode_supported(
        VH_KIND_INSTANCE,
        b'I',
        false,
        "GET_AND_ADD"
    ));
    assert!(!vh_access_mode_supported(
        VH_KIND_INSTANCE,
        b'Z',
        false,
        "GET_AND_ADD"
    ));
    assert!(vh_access_mode_supported(
        VH_KIND_INSTANCE,
        b'Z',
        false,
        "GET_AND_BITWISE_OR"
    ));
    assert!(!vh_access_mode_supported(
        VH_KIND_STATIC,
        b'L',
        false,
        "GET_AND_BITWISE_OR"
    ));
    assert!(vh_access_mode_supported(
        VH_KIND_ARRAY,
        b'L',
        false,
        "COMPARE_AND_SET"
    ));
    assert!(vh_access_mode_supported(
        VH_KIND_INSTANCE,
        b'I',
        true,
        "GET_VOLATILE"
    ));
    assert!(!vh_access_mode_supported(
        VH_KIND_INSTANCE,
        b'I',
        true,
        "SET"
    ));
    assert!(!vh_access_mode_supported(
        VH_KIND_INSTANCE,
        b'I',
        true,
        "GET_AND_SET"
    ));
    // A byte[] view: get and set only.
    assert!(vh_access_mode_supported(
        VH_KIND_BYTE_VIEW_LE,
        b'I',
        false,
        "GET"
    ));
    assert!(!vh_access_mode_supported(
        VH_KIND_BYTE_VIEW_BE,
        b'I',
        false,
        "GET_VOLATILE"
    ));
    assert!(!vh_access_mode_supported(
        VH_KIND_BYTE_VIEW_LE,
        b'J',
        false,
        "COMPARE_AND_SET"
    ));
    // A ByteBuffer view, by element type.
    assert!(vh_access_mode_supported(
        VH_KIND_BYTE_BUFFER_VIEW_LE,
        b'I',
        false,
        "GET_AND_ADD"
    ));
    assert!(!vh_access_mode_supported(
        VH_KIND_BYTE_BUFFER_VIEW_LE,
        b'D',
        false,
        "GET_AND_ADD"
    ));
    assert!(vh_access_mode_supported(
        VH_KIND_BYTE_BUFFER_VIEW_BE,
        b'D',
        false,
        "COMPARE_AND_SET"
    ));
    assert!(vh_access_mode_supported(
        VH_KIND_BYTE_BUFFER_VIEW_LE,
        b'S',
        false,
        "GET_ACQUIRE"
    ));
    assert!(!vh_access_mode_supported(
        VH_KIND_BYTE_BUFFER_VIEW_LE,
        b'C',
        false,
        "COMPARE_AND_SET"
    ));
}

#[test]
fn varhandle_instance_refusal_judges_array_typed_fields() {
    fn refuses(
        ctx: &mut MockNativeContext,
        meta: &VarHandleMeta,
        recv: ObjectRef,
        value: Option<ObjectRef>,
    ) -> bool {
        vh_instance_refusal(ctx, Some(meta), recv, Some(Value::Object(value))).is_some()
    }
    let mut ctx = MockNativeContext::new();
    let recv = match ctx.new_object("Box").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    let cid = ctx.class_id_of_object(recv).as_u32();
    let meta = |desc: &str| VarHandleMeta {
        kind: VH_KIND_INSTANCE,
        class_name: "Box".to_string(),
        field_name: "arr".to_string(),
        field_desc: desc.to_string(),
        field_index: 0,
        class_id: cid,
        read_only: false,
        exact: None,
        ref_type_class_id: 0,
        holder_class_id: 0,
    };
    let string = match ctx.new_object("java/lang/String").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    let ints = ctx.new_array(ArrayElementType::Int, 2);
    let longs = ctx.new_array(ArrayElementType::Long, 2);
    let refs = ctx.new_array(ArrayElementType::Reference, 2);

    let int_arr = meta("[I");
    assert!(
        refuses(&mut ctx, &int_arr, recv, Some(string)),
        "String into int[]"
    );
    assert!(
        refuses(&mut ctx, &int_arr, recv, Some(longs)),
        "long[] into int[]"
    );
    assert!(
        refuses(&mut ctx, &int_arr, recv, Some(refs)),
        "Object[] into int[]"
    );
    assert!(
        !refuses(&mut ctx, &int_arr, recv, Some(ints)),
        "int[] into int[]"
    );
    assert!(!refuses(&mut ctx, &int_arr, recv, None), "null into int[]");

    let obj_arr = meta("[Ljava/lang/Object;");
    assert!(
        refuses(&mut ctx, &obj_arr, recv, Some(ints)),
        "int[] into Object[]"
    );
    assert!(
        refuses(&mut ctx, &obj_arr, recv, Some(string)),
        "String into Object[]"
    );
    assert!(
        !refuses(&mut ctx, &obj_arr, recv, Some(refs)),
        "Object[] into Object[]"
    );

    let int_arr_2d = meta("[[I");
    assert!(
        refuses(&mut ctx, &int_arr_2d, recv, Some(ints)),
        "int[] into int[][]"
    );
    assert!(
        !refuses(&mut ctx, &int_arr_2d, recv, Some(refs)),
        "no VM behind the mock to judge the component: fail open",
    );

    let string_arr = meta("[Ljava/lang/String;");
    assert!(
        !refuses(&mut ctx, &string_arr, recv, Some(refs)),
        "no VM behind the mock to judge the component: fail open",
    );
    assert!(
        refuses(&mut ctx, &string_arr, recv, Some(ints)),
        "int[] into String[]"
    );

    // r11w4-rt-varhandle-array-element-stores-unchecked: the element-store
    // check refuses only on a positive answer. A primitive array, a null,
    // and a context with no hierarchy (`aastore_element_assignable` ==
    // `None`, no readable `componentType`) all store.
    assert!(vh_array_store_refusal(&mut ctx, recv, ints, Value::Int(7)).is_none());
    assert!(vh_array_store_refusal(&mut ctx, recv, refs, Value::Object(None)).is_none());
    assert!(
        vh_array_store_refusal(&mut ctx, recv, refs, Value::Object(Some(string))).is_none(),
        "no hierarchy to consult: must not refuse",
    );
}

/// JIT round 21 wave 26 (lane vhase): only plain `set` casts the value
/// before its index check; `setVolatile` / `setRelease` / `setOpaque`
/// check the index first (`R21Fz9VhArrRef1` `wrong@-1`), unless
/// `CRATONVM_VH_ARRAY_CAST_BEFORE_INDEX_SET_ONLY=0`.
/// JIT round 21 wave 26 (lane vhase): `runtimeTypeCheck`'s slow path is
/// taken only on a KNOWN header component that differs from the handle's
/// (`String[]` through a `CharSequence[]` handle); an unknown (`0`) header
/// keeps the fast path's cast.
#[test]
fn the_covariant_slow_path_needs_a_known_different_component() {
    assert!(vh_array_header_is_covariant(7, 9));
    assert!(!vh_array_header_is_covariant(9, 9));
    assert!(!vh_array_header_is_covariant(0, 9));
    // JIT round 22 wave 1, lane review: an ARRAY component is decided by
    // the array's own class name against `[` + the component's.
    assert!(vh_array_name_is_covariant("[[Ljava/lang/String;", "[Ljava/lang/Object;"));
    assert!(!vh_array_name_is_covariant("[[Ljava/lang/Object;", "[Ljava/lang/Object;"));
    assert!(!vh_array_name_is_covariant("[[Ljava/lang/Object;", "[Ljava.lang.Object;"));
    assert!(!vh_array_name_is_covariant("[[I", "[I"));
}

#[test]
fn only_plain_set_casts_its_value_before_the_index_check() {
    assert!(vh_array_mode_casts_before_index_with("set", true, true));
    for mode in ["setVolatile", "setRelease", "setOpaque"] {
        assert!(!vh_array_mode_casts_before_index_with(mode, true, true), "{mode}");
        assert!(vh_array_mode_casts_before_index_with(mode, true, false), "{mode}");
        assert!(!vh_array_mode_casts_before_index_with(mode, false, false), "{mode}");
    }
    assert!(!vh_array_mode_casts_before_index_with("set", false, true));
}

/// JIT round 21 wave 25 (lane vhase): plain `set` is an `aastore` in JDK
/// 25's `VarHandleReferences.Array` (class-name message); every other
/// store mode is `reflectiveTypeCheck`'s `new ArrayStoreException()`
/// (empty here, a null `getMessage()`), unless the switch is off.
#[test]
fn a_runtime_type_check_array_store_refusal_has_no_message() {
    let text = || "java.lang.Integer".to_string();
    let msg = |e: RuntimeError| match e {
        RuntimeError::ArrayStoreException { message } => message,
        other => panic!("not an ArrayStoreException: {other:?}"),
    };
    assert_eq!(msg(vh_array_store_check_error(true, true, text)), "java.lang.Integer");
    assert_eq!(msg(vh_array_store_check_error(false, true, text)), "");
    assert_eq!(msg(vh_array_store_check_error(false, false, text)), "java.lang.Integer");
    assert_eq!(msg(vh_array_store_check_error(true, false, text)), "java.lang.Integer");
}

/// r11w7-rt-varhandle-ref-array-field-check-patch: a reference array into
/// a reference-array field is judged by the VM's `checkcast` predicate
/// (`array_value_casts_to_desc`), and ONLY that case consults it. Before,
/// the arm answered `None` unconditionally, so `VH.set(h, (Object) new
/// Integer[1])` into a `String[]` field stored the `Integer[]`.
#[test]
fn a_reference_array_into_a_reference_array_field_takes_the_vms_cast_verdict() {
    let mut ctx = MockNativeContext::new();
    let refs = ctx.new_array(ArrayElementType::Reference, 1);
    let ints = ctx.new_array(ArrayElementType::Int, 1);
    let string_arr = "[Ljava/lang/String;";

    // The VM's verdict is passed through unchanged: a positive mismatch
    // refuses, a match admits, and "cannot tell" fails open.
    for verdict in [Some(false), Some(true), None] {
        let mut asked: Option<(ObjectRef, String)> = None;
        let got = vh_array_field_value_admitted_with(&ctx, refs, string_arr, |v, d| {
            asked = Some((v, d.to_string()));
            verdict
        });
        assert_eq!(got, verdict, "verdict {verdict:?}");
        assert_eq!(
            asked,
            Some((refs, string_arr.to_string())),
            "the value and the FIELD descriptor are what the VM is asked about",
        );
    }
    // `[[I` is a reference-array field too: an `int[][]` value is a
    // reference array whose component the VM judges.
    assert_eq!(
        vh_array_field_value_admitted_with(&ctx, refs, "[[I", |_, _| Some(false)),
        Some(false),
    );

    // Everything the heap shape settles never reaches the VM question.
    let mut consulted = false;
    let mut ask = |_: ObjectRef, _: &str| {
        consulted = true;
        Some(true)
    };
    assert_eq!(
        vh_array_field_value_admitted_with(&ctx, ints, string_arr, &mut ask),
        Some(false),
        "int[] into String[]",
    );
    assert_eq!(
        vh_array_field_value_admitted_with(&ctx, refs, "[I", &mut ask),
        Some(false),
        "Object[] into int[]",
    );
    assert_eq!(
        vh_array_field_value_admitted_with(&ctx, ints, "[I", &mut ask),
        Some(true),
        "int[] into int[]",
    );
    assert_eq!(
        vh_array_field_value_admitted_with(&ctx, refs, "Ljava/lang/String;", &mut ask),
        None,
        "not an array-typed field",
    );
    assert!(
        !consulted,
        "the VM predicate is asked only for ref-array into ref-array"
    );

    // The production entry, on a context with no VM behind it, still fails
    // open (the mock's `array_value_casts_to_desc` is the default `None`).
    assert_eq!(vh_array_field_value_admitted(&ctx, refs, string_arr), None);
}

#[test]
fn varhandle_instance_refusal_type_checks() {
    let mut ctx = MockNativeContext::new();
    let recv = match ctx.new_object("Box").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    let cid = ctx.class_id_of_object(recv).as_u32();
    let bad_str = match ctx.new_object("java/lang/String").unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    let int_meta = VarHandleMeta {
        kind: VH_KIND_INSTANCE,
        class_name: "Box".to_string(),
        field_name: "i".to_string(),
        field_desc: "I".to_string(),
        field_index: 0,
        class_id: cid,
        read_only: false,
        exact: None,
        ref_type_class_id: 0,
        holder_class_id: 0,
    };
    // 1. Primitive field receiving non-wrapper reference must refuse with WrongMethodTypeException
    let bad_prim = vh_instance_refusal(
        &mut ctx,
        Some(&int_meta),
        recv,
        Some(Value::Object(Some(bad_str))),
    );
    assert!(
        bad_prim.is_some(),
        "non-wrapper reference into primitive field must refuse"
    );

    // 2. Matching receiver with valid primitive value must pass
    let good_prim = vh_instance_refusal(&mut ctx, Some(&int_meta), recv, Some(Value::Int(42)));
    assert!(
        good_prim.is_none(),
        "valid primitive value into matching receiver must not refuse"
    );

    // 3. None meta or empty class name must not refuse
    assert!(
        vh_instance_refusal(&mut ctx, None, recv, Some(Value::Int(42))).is_none(),
        "None meta must not refuse"
    );
    let unclassed_meta = VarHandleMeta {
        kind: VH_KIND_INSTANCE,
        class_name: String::new(),
        field_name: "i".to_string(),
        field_desc: "I".to_string(),
        field_index: 0,
        class_id: 0,
        read_only: false,
        exact: None,
        ref_type_class_id: 0,
        holder_class_id: 0,
    };
    assert!(
        vh_instance_refusal(&mut ctx, Some(&unclassed_meta), recv, Some(Value::Int(42)))
            .is_none(),
        "unclassed meta must not refuse"
    );
}

fn is_class_cast(err: &MethodCallFailed) -> bool {
    matches!(
        err,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { .. }
        ))
    )
}

/// r11w13-orch-varhandle-set-on-an-array-receiver-throws-aioobe-not-cce: an
/// array's header carries its COMPONENT's class id, so an `H[]` receiver
/// passed the `meta.class_id` compare for an `H` handle, and
/// `F.set((Object) new H[1], i)` was taken for an ARRAY-ELEMENT call by its
/// `[vh, array, int]` shape. HotSpot: `ClassCastException` for every arm.
/// The mock gives every array class id 0, so a meta naming class id 0 is
/// exactly the alias.
#[test]
fn an_array_receiver_is_never_an_instance_field_receiver() {
    let mut ctx = boxing_ctx();
    let vh = vh_with_garbage_kind(&mut ctx);
    let arr = ctx.new_array(ArrayElementType::Reference, 1);
    let before = ctx.get_array_element(arr, 0);
    let alias_cid = ctx.class_id_of_object(arr).as_u32();
    let meta = VarHandleMeta {
        kind: VH_KIND_INSTANCE,
        class_name: "H".to_string(),
        field_name: "f".to_string(),
        field_desc: "I".to_string(),
        field_index: 0,
        class_id: alias_cid,
        read_only: false,
        exact: None,
        ref_type_class_id: 0,
        holder_class_id: 0,
    };
    vh_meta_put(&mut ctx, vh, meta.clone());

    // The predicate: an array is refused as an `H`, however its id reads.
    let refusal = vh_instance_refusal(&mut ctx, Some(&meta), arr, None)
        .expect("an H[] receiver must be refused for an H handle");
    assert!(is_class_cast(&refusal));

    // set: index 0 is IN range, so the old array-element routing stored
    // the int into the array and answered normally.
    let set = varhandle_set(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(arr)),
            Value::Int(0),
        ],
    );
    assert!(set.as_ref().is_err_and(is_class_cast), "set: {set:?}");
    assert_eq!(
        ctx.get_array_element(arr, 0),
        before,
        "the array must not be written"
    );

    // get: the instance arm read field 0 out of the array.
    let get = varhandle_get(
        &mut ctx,
        &[Value::Object(Some(vh)), Value::Object(Some(arr))],
    );
    assert!(get.as_ref().is_err_and(is_class_cast), "get: {get:?}");

    // compareAndSet shares the routing.
    let cas = varhandle_compare_and_set(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(7),
        ],
    );
    assert!(cas.as_ref().is_err_and(is_class_cast), "cas: {cas:?}");
    assert_eq!(
        ctx.get_array_element(arr, 0),
        before,
        "the array must not be written"
    );
}

/// The VALUE half of the same alias: an `Integer[]` read as an `Integer`
/// and was stored into an `Integer` field. An array is an instance of
/// `Object`, `Cloneable` and `Serializable` only.
#[test]
fn an_array_is_only_an_object_cloneable_or_serializable() {
    let mut ctx = MockNativeContext::new();
    let arr = ctx.new_array(ArrayElementType::Reference, 1);
    let v = Value::Object(Some(arr));
    assert_eq!(
        reference_arg_admitted(&mut ctx, v, "Ljava/lang/Integer;"),
        Some(false)
    );
    assert_eq!(
        reference_arg_admitted(&mut ctx, v, "Ljava/lang/Cloneable;"),
        Some(true)
    );
    assert_eq!(
        reference_arg_admitted(&mut ctx, v, "Ljava/io/Serializable;"),
        Some(true)
    );
    assert_eq!(
        reference_arg_admitted(&mut ctx, v, "Ljava/lang/Object;"),
        None
    );
    assert_eq!(
        reference_arg_admitted(&mut ctx, v, "[Ljava/lang/Integer;"),
        None
    );

    let meta = VarHandleMeta {
        kind: VH_KIND_INSTANCE,
        class_name: "H".to_string(),
        field_name: "g".to_string(),
        field_desc: "Ljava/lang/Integer;".to_string(),
        field_index: 1,
        class_id: 0,
        read_only: false,
        exact: None,
        ref_type_class_id: 0,
        holder_class_id: 0,
    };
    let refusal =
        vh_value_refusal(&mut ctx, &meta, v).expect("Integer[] into an Integer field");
    assert!(is_class_cast(&refusal));
    match refusal {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { message },
        )) => assert!(
            message.starts_with("Cannot cast [L"),
            "the ARRAY is named, not its component: {message}"
        ),
        other => panic!("{other:?}"),
    }
}

// -----------------------------------------------------------------------
// `Lookup.find*` access control — the gate that RUNS
// -----------------------------------------------------------------------
//
// These arrived here on 2026-08-12 from `classloader.rs`, where five of
// them had been green since W3-1 aimed at `classloader::lk_find_virtual` /
// `lk_find_getter` — bodies no registrar ever registered, because
// `register_p63_method_handles_lookup` in THIS file owns all ten `find*`
// triples and `classloader.rs`'s registration site says in its own comment
// that it must not re-register them. So the check they asserted about was
// not the check the VM ran, and `publicLookup().findVirtual(_, <private>)`
// reached the private method while its own unit test said it could not.
// That is the shape W4-1-publiclookup-allowedmodes-never-checked.md is
// named after, and a test guarding the wrong copy is how it shipped.
//
// Re-pointed at [`lk_enforce_find_access`], the first statement of all ten
// `lookup_find_*`. Four arms the old tests could not express are added,
// and each exists because it is a way this gate can be wrong WITHOUT any
// of the others noticing:
//
//   * a real `publicLookup()` mode word. The old tests passed
//     `LK_PUBLIC` (0x01). `publicLookup()` is measured at **0x20**,
//     `UNCONDITIONAL` and nothing else — a lookup that does not carry the
//     `PUBLIC` bit at all. Every one of those five tests was therefore
//     asserting about a Lookup shape the JDK never hands out.
//   * the `UNCONDITIONAL` class rule: a PUBLIC member of a NON-public
//     class is refused, because that mode's rule is about the target class.
//   * a zero-mode Lookup, refused before the member walk.
//   * an UNREADABLE mode word, which must stay permissive. `Some(0)` and
//     `None` are different answers here and a gate that collapsed them
//     would still pass every other test in this block.
//
// And the positive private case, which `classloader.rs`'s test module
// carried a NOTE saying could not be exercised in-unit ("`lk_modes_of`
// always reports mode 0 under the mock"). It can: the mock has no
// `allowedModes` entry in `mock_field_slot` and no declared field for it,
// so the class-side witness answers `None` and the reader falls through to
// the synthetic slot the test wrote. The note described a reader that had
// already been replaced. W7-62-ratchets-and-dead-code.md

use cratonvm_native_api::{FieldMetadata, MethodMetadata};
use cratonvm_types::ClassId;

/// A target class carrying one method `secret` and one field `hidden` with
/// the given access flags, plus the CLASS's own access flags — public
/// unless a test says otherwise, because `UNCONDITIONAL` asks about the
/// class and the mock's default is 0 (package-private).
fn lk_target(
    ctx: &mut MockNativeContext,
    class_name: &str,
    class_flags: u16,
    method_flags: u16,
    field_flags: u16,
) -> ObjectRef {
    let cls_id = ctx.ensure_class_initialized(class_name).unwrap();
    ctx.set_class_access_flags(cls_id, class_flags);
    ctx.set_declared_methods(
        cls_id,
        vec![MethodMetadata {
            name: "secret".to_string(),
            descriptor: "()V".to_string(),
            access_flags: method_flags,
            declaring_class_id: cls_id,
            exceptions: Vec::new(),
            signature: None,
        }],
    );
    ctx.set_declared_fields(
        cls_id,
        vec![FieldMetadata {
            name: "hidden".to_string(),
            descriptor: "I".to_string(),
            access_flags: field_flags,
            slot_index: 0,
            declaring_class_id: cls_id,
            is_static: false,
        }],
    );
    ctx.get_class_mirror(cls_id)
}

/// A `Lookup` receiver whose `allowedModes` reads back as `modes`.
///
/// Writes the SYNTHETIC slot, which is what
/// [`lk_read_allowed_modes_opt`] falls through to when the class-side
/// witness finds no declared `allowedModes` — the fabricated-Lookup shape.
fn lk_with_modes(ctx: &mut MockNativeContext, modes: i32) -> ObjectRef {
    let lk = ctx.alloc_object(ClassId::new(0), 4);
    ctx.set_field(lk, LK_SYNTHETIC_ALLOWED_MODES, Value::Int(modes));
    lk
}

/// `args` as `Lookup.findVirtual(refc, name, type)` hands them over.
fn lk_find_args(lk: ObjectRef, target: ObjectRef, name: ObjectRef) -> [Value; 4] {
    [
        Value::Object(Some(lk)),
        Value::Object(Some(target)),
        Value::Object(Some(name)),
        Value::Object(None),
    ]
}

const ACC_PUBLIC_U16: u16 = cratonvm_types::access_flags::ACC_PUBLIC;
const ACC_PRIVATE_U16: u16 = cratonvm_types::access_flags::ACC_PRIVATE;

/// THE DEFECT W4-1 IS NAMED AFTER. `publicLookup()` must not reach a
/// private method of a public class.
#[test]
fn publiclookup_is_refused_a_private_method() {
    let mut ctx = MockNativeContext::new();
    let target = lk_target(
        &mut ctx,
        "p/Target",
        ACC_PUBLIC_U16,
        ACC_PRIVATE_U16,
        ACC_PUBLIC_U16,
    );
    let lk = lk_with_modes(&mut ctx, LK_MODE_UNCONDITIONAL);
    let name = ctx.create_string("secret");
    let r = lk_enforce_find_access(
        &ctx,
        &lk_find_args(lk, target, name),
        2,
        None,
        false,
        "invokeVirtual",
    );
    assert!(
        r.is_err(),
        "publicLookup (modes 0x20) must not reach a private method; got {r:?}"
    );
    // Wave 48: a METHOD through `publicLookup()` fails HotSpot's own
    // resolution, `no such method: <member>`, with no `from` clause.
    let refusal = r.unwrap_err();
    assert!(
        refusal.head.starts_with("no such method: p.Target.secret")
            && refusal.head.ends_with("/invokeVirtual")
            && refusal.from.is_none(),
        "got {refusal:?}"
    );
}

/// Calibration for the test above: the same lookup, the same class, a
/// PUBLIC method. Without this row a gate that refused everything would
/// look correct.
#[test]
fn publiclookup_reaches_a_public_method_of_a_public_class() {
    let mut ctx = MockNativeContext::new();
    let target = lk_target(
        &mut ctx,
        "p/Target",
        ACC_PUBLIC_U16,
        ACC_PUBLIC_U16,
        ACC_PUBLIC_U16,
    );
    let lk = lk_with_modes(&mut ctx, LK_MODE_UNCONDITIONAL);
    let name = ctx.create_string("secret");
    let r = lk_enforce_find_access(
        &ctx,
        &lk_find_args(lk, target, name),
        2,
        None,
        false,
        "invokeVirtual",
    );
    assert!(
        r.is_ok(),
        "a public member of a public class must resolve; got {r:?}"
    );
}

/// `UNCONDITIONAL`'s rule is about the target CLASS. A public member of a
/// package-private class is refused — measured on OpenJDK 25.0.3 as
/// `IllegalAccessException: symbolic reference class is not accessible`.
#[test]
fn publiclookup_is_refused_a_public_member_of_a_non_public_class() {
    let mut ctx = MockNativeContext::new();
    // class flags 0 = package-private.
    let target = lk_target(&mut ctx, "p/Packaged", 0, ACC_PUBLIC_U16, ACC_PUBLIC_U16);
    let lk = lk_with_modes(&mut ctx, LK_MODE_UNCONDITIONAL);
    let name = ctx.create_string("secret");
    let r = lk_enforce_find_access(
        &ctx,
        &lk_find_args(lk, target, name),
        2,
        None,
        false,
        "invokeVirtual",
    );
    assert!(
        r.is_err(),
        "UNCONDITIONAL reaches public members of PUBLIC types only; got {r:?}"
    );
}

/// The field half of the same boundary — `findGetter`/`findSetter` pass
/// `is_field = true`, and the two halves resolve members differently
/// (`lk_member_access_flags` walks superclasses for methods and not for
/// fields), so one passing does not imply the other.
#[test]
fn publiclookup_is_refused_a_private_field_getter() {
    let mut ctx = MockNativeContext::new();
    let target = lk_target(
        &mut ctx,
        "p/Target",
        ACC_PUBLIC_U16,
        ACC_PUBLIC_U16,
        ACC_PRIVATE_U16,
    );
    let lk = lk_with_modes(&mut ctx, LK_MODE_UNCONDITIONAL);
    let name = ctx.create_string("hidden");
    let r = lk_enforce_find_access(
        &ctx,
        &lk_find_args(lk, target, name),
        2,
        None,
        true,
        "getField",
    );
    assert!(
        r.is_err(),
        "private field getter via publicLookup must throw; got {r:?}"
    );
    // Wave 48: HotSpot's `checkAccess` text, with the `from` clause of
    // `publicLookup()` (a METHOD through it is `no such method`, below).
    let refusal = r.unwrap_err();
    assert!(
        refusal
            .head
            .starts_with("member is private: p.Target.hidden/")
            && refusal.head.ends_with("/getField"),
        "got {refusal:?}"
    );
    assert!(
        matches!(refusal.from, Some(LkFrom::Public)),
        "got {refusal:?}"
    );
}

#[test]
fn publiclookup_reaches_a_public_field_getter() {
    let mut ctx = MockNativeContext::new();
    let target = lk_target(
        &mut ctx,
        "p/Target",
        ACC_PUBLIC_U16,
        ACC_PUBLIC_U16,
        ACC_PUBLIC_U16,
    );
    let lk = lk_with_modes(&mut ctx, LK_MODE_UNCONDITIONAL);
    let name = ctx.create_string("hidden");
    let r = lk_enforce_find_access(
        &ctx,
        &lk_find_args(lk, target, name),
        2,
        None,
        true,
        "getField",
    );
    assert!(r.is_ok(), "public field getter must resolve; got {r:?}");
}

/// A full-power `MethodHandles.lookup()` reaches a private member. The
/// positive case `classloader.rs`'s test module said could not be
/// exercised in-unit.
#[test]
fn a_full_power_lookup_reaches_a_private_member() {
    let mut ctx = MockNativeContext::new();
    let target = lk_target(
        &mut ctx,
        "p/Target",
        ACC_PUBLIC_U16,
        ACC_PRIVATE_U16,
        ACC_PUBLIC_U16,
    );
    let lk = lk_with_modes(&mut ctx, LK_MODE_FULL_POWER);
    let name = ctx.create_string("secret");
    let r = lk_enforce_find_access(
        &ctx,
        &lk_find_args(lk, target, name),
        2,
        None,
        false,
        "invokeVirtual",
    );
    assert!(
        r.is_ok(),
        "a lookup holding PRIVATE must reach a private member; got {r:?}"
    );
}

/// `lookup().dropLookupMode(PUBLIC)` is 0 on the real JDK, and a zero-mode
/// Lookup is refused EVERY member including a public one — before the
/// member walk, which is why the class is public and the member is public
/// here and it still throws.
#[test]
fn a_zero_mode_lookup_is_refused_even_a_public_member() {
    let mut ctx = MockNativeContext::new();
    let target = lk_target(
        &mut ctx,
        "p/Target",
        ACC_PUBLIC_U16,
        ACC_PUBLIC_U16,
        ACC_PUBLIC_U16,
    );
    let lk = lk_with_modes(&mut ctx, 0);
    let name = ctx.create_string("secret");
    let r = lk_enforce_find_access(
        &ctx,
        &lk_find_args(lk, target, name),
        2,
        None,
        false,
        "invokeVirtual",
    );
    assert!(r.is_err(), "a zero-mode Lookup must be refused; got {r:?}");
}

/// ...and the receiver whose mode word this VM CANNOT READ must stay
/// permissive. `Some(0)` and `None` are different answers, and collapsing
/// them turns every Lookup shape the VM does not model into an
/// `IllegalAccessException`. Every other test in this block would still
/// pass with them collapsed, which is why this row is here.
#[test]
fn an_unreadable_mode_word_stays_permissive() {
    let mut ctx = MockNativeContext::new();
    let target = lk_target(
        &mut ctx,
        "p/Target",
        ACC_PUBLIC_U16,
        ACC_PRIVATE_U16,
        ACC_PUBLIC_U16,
    );
    let lk = ctx.alloc_object(ClassId::new(0), 4);
    // Not an `Int` in either place the reader looks: no declared
    // `allowedModes` on this class, and a reference in the synthetic slot.
    ctx.set_field(lk, LK_SYNTHETIC_ALLOWED_MODES, Value::Object(None));
    let name = ctx.create_string("secret");
    let r = lk_enforce_find_access(
        &ctx,
        &lk_find_args(lk, target, name),
        2,
        None,
        false,
        "invokeVirtual",
    );
    assert!(
        r.is_ok(),
        "an unreadable mode word must not become a refusal; got {r:?}"
    );
}

/// A member whose flags cannot be resolved at all must be ALLOWED, so a
/// class this VM does not model is never spuriously refused.
#[test]
fn an_unresolvable_member_is_not_blocked() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx.ensure_class_initialized("p/Opaque").unwrap();
    ctx.set_class_access_flags(cid, ACC_PUBLIC_U16);
    let target = ctx.get_class_mirror(cid);
    let lk = lk_with_modes(&mut ctx, LK_MODE_UNCONDITIONAL);
    let name = ctx.create_string("whatever");
    let r = lk_enforce_find_access(
        &ctx,
        &lk_find_args(lk, target, name),
        2,
        None,
        false,
        "invokeVirtual",
    );
    assert!(
        r.is_ok(),
        "an unresolvable member must not be blocked; got {r:?}"
    );
}

/// Methods resolve up the superclass chain; fields do not. Moved from
/// `classloader.rs`, where it exercised that module's own now-deleted copy
/// of this helper.
#[test]
fn lk_member_access_flags_walks_superclass_for_methods() {
    let mut ctx = MockNativeContext::new();
    let parent = ctx.ensure_class_initialized("p/Parent").unwrap();
    ctx.set_declared_methods(
        parent,
        vec![MethodMetadata {
            name: "inherited".to_string(),
            descriptor: "()V".to_string(),
            access_flags: ACC_PUBLIC_U16,
            declaring_class_id: parent,
            exceptions: Vec::new(),
            signature: None,
        }],
    );
    let child = ctx.ensure_class_initialized("p/Child").unwrap();
    ctx.set_declared_methods(child, Vec::new());
    ctx.set_superclass(child, parent);
    let mirror = ctx.get_class_mirror(child);
    assert_eq!(
        lk_member_access_flags(&ctx, mirror, "inherited", false),
        Some(ACC_PUBLIC_U16)
    );
}

// -----------------------------------------------------------------------
// G31 — `MethodHandle.asType` CONVERTIBILITY
//
// Every assertion below is a cell of a sweep MEASURED on HotSpot
// 25.0.3+9-LTS (`scratchpad/g31/AsTypeFamily.java`, `AsTypeExtra.java`;
// 613 + 304 cells). The predicate is a pure function of descriptor
// strings, so the whole rule is testable with no mock and no VM — which is
// the only reason it could be checked at all by a lane forbidden to build.
//
// The rows chosen here are the ones a plausible WRONG implementation
// passes the rest of the matrix while failing: the two direction traps,
// the three primitive asymmetries, the `ConstantDesc` split, and the
// reference-to-reference blanket accept.
// -----------------------------------------------------------------------

/// The blanket rule that is easiest to disbelieve: `asType` accepts EVERY
/// reference-to-reference pair, however unrelated, because the cast is
/// deferred to invoke time and `null` is always dynamically valid.
#[test]
fn reference_to_reference_is_always_convertible() {
    // MEASURED: R String -> Integer = ok, R int[] -> String = ok,
    //           R Void -> Comparable = ok
    for (a, b) in [
        ("Ljava/lang/String;", "Ljava/lang/Integer;"),
        ("[I", "Ljava/lang/String;"),
        ("Ljava/lang/Void;", "Ljava/lang/Comparable;"),
        ("Ljava/lang/Runnable;", "[[Ljava/lang/Object;"),
    ] {
        assert!(
            mh_can_convert(a, b),
            "MEASURED ok on HotSpot: {a} -> {b} must be convertible"
        );
    }
}

/// `void` is convertible in BOTH directions as a return type — the value is
/// dropped one way, a zero/null introduced the other. The whole `void` row
/// and the whole `void` column of the measured return matrix are accepts.
#[test]
fn void_converts_in_both_directions() {
    for t in ["Z", "I", "D", "Ljava/lang/String;", "[I"] {
        assert!(mh_can_convert("V", t), "MEASURED: void -> {t} = ok");
        assert!(mh_can_convert(t, "V"), "MEASURED: {t} -> void = ok");
    }
}

/// The three primitive asymmetries. Each of them is a cell an
/// "any primitive to any primitive" rule gets wrong.
#[test]
fn primitive_widening_is_jls_5_1_2_and_not_symmetric() {
    // MEASURED: R byte -> short = ok, R short -> byte = WrongMethodTypeException
    assert!(mh_can_convert("B", "S"));
    assert!(!mh_can_convert("S", "B"));
    // char does NOT widen to short, and short/byte do NOT widen to char.
    // MEASURED: R char -> short / R short -> char / R byte -> char all refuse.
    assert!(!mh_can_convert("C", "S"));
    assert!(!mh_can_convert("S", "C"));
    assert!(!mh_can_convert("B", "C"));
    // ... but char DOES widen to int and up. MEASURED: R char -> int = ok.
    assert!(mh_can_convert("C", "I"));
    // boolean widens to nothing and nothing widens to it.
    // MEASURED: the whole boolean row and column of the primitive block.
    for t in ["B", "C", "S", "I", "J", "F", "D"] {
        assert!(!mh_can_convert("Z", t), "MEASURED: boolean -> {t} refuses");
        assert!(!mh_can_convert(t, "Z"), "MEASURED: {t} -> boolean refuses");
    }
}

/// The reference-to-primitive arm has THREE tests in the JDK, and the third
/// one — unbox from a strongly typed wrapper, then widen — is the one that
/// is easy to leave out. Leaving it out turns 20 measured accepts into
/// refusals.
#[test]
fn a_wrapper_source_may_unbox_and_then_widen() {
    // MEASURED: U Byte -> short/int/long/float/double = Y, Byte -> char = n
    assert!(mh_can_convert("Ljava/lang/Byte;", "S"));
    assert!(mh_can_convert("Ljava/lang/Byte;", "D"));
    assert!(!mh_can_convert("Ljava/lang/Byte;", "C"));
    // MEASURED: U Character -> int = Y, Character -> short = n
    assert!(mh_can_convert("Ljava/lang/Character;", "I"));
    assert!(!mh_can_convert("Ljava/lang/Character;", "S"));
    // MEASURED: U Double -> double = Y, Double -> float = n (narrowing)
    assert!(mh_can_convert("Ljava/lang/Double;", "D"));
    assert!(!mh_can_convert("Ljava/lang/Double;", "F"));
}

/// The supertype table, and the row that inspection gets wrong: `Byte` and
/// `Short` are `Constable` but NOT `ConstantDesc`.
#[test]
fn the_wrapper_supertype_table_matches_the_measured_rows() {
    // MEASURED: U Number -> byte = Y but Number -> char = n and
    //           Number -> boolean = n (no Number subclass wraps either)
    assert!(mh_can_convert("Ljava/lang/Number;", "B"));
    assert!(!mh_can_convert("Ljava/lang/Number;", "C"));
    assert!(!mh_can_convert("Ljava/lang/Number;", "Z"));
    // MEASURED: U ConstantDesc -> int/long/float/double = Y, byte/short = n
    assert!(mh_can_convert("Ljava/lang/constant/ConstantDesc;", "I"));
    assert!(!mh_can_convert("Ljava/lang/constant/ConstantDesc;", "B"));
    // MEASURED: B byte -> Constable = Y but byte -> ConstantDesc = n
    assert!(mh_can_convert("B", "Ljava/lang/constant/Constable;"));
    assert!(!mh_can_convert("B", "Ljava/lang/constant/ConstantDesc;"));
    // MEASURED: U Comparable/Serializable/Constable -> every primitive = Y
    for r in [
        "Ljava/lang/Comparable;",
        "Ljava/io/Serializable;",
        "Ljava/lang/constant/Constable;",
    ] {
        for p in ["Z", "B", "C", "S", "I", "J", "F", "D"] {
            assert!(mh_can_convert(r, p), "MEASURED: {r} -> {p} = Y");
        }
    }
    // MEASURED: U CharSequence / Cloneable / String / Void -> every
    // primitive = n. None of them is a supertype of any wrapper.
    for r in [
        "Ljava/lang/CharSequence;",
        "Ljava/lang/Cloneable;",
        "Ljava/lang/String;",
        "Ljava/lang/Void;",
    ] {
        for p in ["Z", "B", "C", "S", "I", "J", "F", "D"] {
            assert!(!mh_can_convert(r, p), "MEASURED: {r} -> {p} = n");
        }
    }
}

/// The direction trap. The RETURN travels old -> new; each PARAMETER
/// travels new -> old. A reversed implementation passes every widening row
/// and fails every narrowing one, which reads like an off-by-one.
#[test]
fn parameters_convert_backwards_and_the_return_forwards() {
    // A callee that wants a `long` can be fed by a caller offering an
    // `int`; the reverse is a narrowing and refuses.
    // MEASURED: A int -> long = ok, A long -> int = WrongMethodTypeException
    assert_eq!(method_type_is_convertible_to("(J)V", "(I)V"), Some(true));
    assert_eq!(method_type_is_convertible_to("(I)V", "(J)V"), Some(false));
    // A callee that returns `int` can satisfy a caller expecting `long`;
    // the reverse refuses. MEASURED: R int -> long = ok, R long -> int = X.
    assert_eq!(method_type_is_convertible_to("()I", "()J"), Some(true));
    assert_eq!(method_type_is_convertible_to("()J", "()I"), Some(false));
}

/// `asType` never adds or drops a parameter. All five measured arity rows
/// refuse, in both directions and at every count.
#[test]
fn arity_must_match_exactly() {
    // MEASURED: cannot convert MethodHandle(int)void to ()void, and the
    // four sibling rows of the same family.
    assert_eq!(method_type_is_convertible_to("(I)V", "()V"), Some(false));
    assert_eq!(method_type_is_convertible_to("(I)V", "(II)V"), Some(false));
    assert_eq!(method_type_is_convertible_to("()V", "(I)V"), Some(false));
    assert_eq!(
        method_type_is_convertible_to("(ILjava/lang/String;)V", "()V"),
        Some(false)
    );
    assert_eq!(
        method_type_is_convertible_to("(ILjava/lang/String;)V", "(ILjava/lang/String;J)V"),
        Some(false)
    );
}

/// The exact row `RJdkProxyIface`'s `refusals` step is missing, and the two
/// neighbours that separate "refuses everything" from "refuses this".
#[test]
fn the_rjdkproxyiface_row() {
    // MEASURED: MethodHandleProxies.asInterfaceInstance(Subtractor.class,
    //   <(String)String>) throws
    //   WrongMethodTypeException: cannot convert MethodHandle(String)String
    //   to (int,int)int
    let target = "(Ljava/lang/String;)Ljava/lang/String;";
    assert_eq!(method_type_is_convertible_to(target, "(II)I"), Some(false));
    assert_eq!(
        format!(
            "cannot convert MethodHandle{} to {}",
            method_type_display(target).unwrap(),
            method_type_display("(II)I").unwrap()
        ),
        "cannot convert MethodHandle(String)String to (int,int)int"
    );
    // MEASURED: the MATCHING handle is accepted and its proxy invokes.
    assert_eq!(method_type_is_convertible_to("(II)I", "(II)I"), Some(true));
    // MEASURED: S subtractor.fromWidening = WrongMethodTypeException —
    // `(long,long)long` is refused even though int->long widens, because
    // the RETURN long->int does not.
    assert_eq!(method_type_is_convertible_to("(JJ)J", "(II)I"), Some(false));
}

/// The message is transcribed from HotSpot, so its rendering is asserted
/// character for character. `MethodType.toString()` uses simple names,
/// arrays keep their brackets, and a nested class prints its inner name
/// alone.
#[test]
fn method_type_display_matches_hotspots_tostring() {
    // MEASURED: T arrays = (String[],Object[][])int[]
    assert_eq!(
        method_type_display("([Ljava/lang/String;[[Ljava/lang/Object;)[I").as_deref(),
        Some("(String[],Object[][])int[]")
    );
    // MEASURED: T nested = (Entry)Inner
    assert_eq!(
        method_type_display("(Ljava/util/Map$Entry;)LAsTypeExtra$Inner;").as_deref(),
        Some("(Entry)Inner")
    );
    // MEASURED: T void = ()void
    assert_eq!(method_type_display("()V").as_deref(), Some("()void"));
    // MEASURED: T prims = (boolean,byte,char,short,int,long,float)double
    assert_eq!(
        method_type_display("(ZBCSIJF)D").as_deref(),
        Some("(boolean,byte,char,short,int,long,float)double")
    );
    // A descriptor this file cannot name yields `None` rather than a
    // signature with a hole in it, so `mh_astype_refusal` declines to
    // compose a half-rendered message.
    assert_eq!(method_type_display("(L;)V"), None);
    assert_eq!(method_type_display("not a descriptor"), None);
}

/// The TRAP: `explicitCastArguments` has different rules. Every type pair
/// `asType` refuses, it accepts — it refuses on arity alone.
#[test]
fn explicit_cast_shares_no_type_rule_with_astype() {
    // MEASURED: X String -> boolean = ok, X double -> char = ok,
    //           X int[] -> long = ok — all three are asType refusals.
    for (a, b) in [
        ("Ljava/lang/String;", "Z"),
        ("D", "C"),
        ("[I", "J"),
        ("Ljava/lang/Void;", "I"),
    ] {
        assert!(
            !mh_can_convert(a, b),
            "asType MUST refuse {a} -> {b} (MEASURED)"
        );
    }
    // The only thing `explicitCastArguments` checks is the parameter count,
    // which is why its refusal is expressed on `split_descriptor_params`
    // and not on `method_type_is_convertible_to`.
    // MEASURED: Z drop1 = cannot explicitly cast MethodHandle(int)void to ()void
    let (old_params, _) = split_descriptor_params("(I)V").unwrap();
    let (new_params, _) = split_descriptor_params("()V").unwrap();
    assert_ne!(old_params.len(), new_params.len());
    assert_eq!(
        format!(
            "cannot explicitly cast MethodHandle{} to {}",
            method_type_display("(I)V").unwrap(),
            method_type_display("()V").unwrap()
        ),
        "cannot explicitly cast MethodHandle(int)void to ()void"
    );
}

#[cfg(test)]
mod vh_plan_memo_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Forget one private VM's meta rows and lock keys, even on a panic.
struct ForgetVm(usize);
impl Drop for ForgetVm {
    fn drop(&mut self) {
        forget_vm_var_handle_meta(self.0);
        crate::forget_vm_lock_keys(self.0);
    }
}

/// A fake, 8-aligned handle address: a key only, never dereferenced (the
/// meta natives hash and key it; the mock's var-handle root is a no-op).
fn at(addr: usize) -> ObjectRef {
    // SAFETY: see above.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

/// File `meta` for the handle at `addr` of `vm` the way the natives do
/// (minting its weak lock key), and answer its identity hash.
fn file(vm: usize, addr: usize, meta: VarHandleMeta) -> i32 {
    let mut ctx = ctx_for(vm);
    vh_meta_put(&mut ctx, at(addr), meta);
    ctx.identity_hash_code(at(addr))
}

/// A memoised NEGATIVE must not survive the resolve-on-first-use
/// transition. This is the dangerous direction: the plan lookup answers
/// `None` for a handle whose field slot is not resolved yet, the funnel
/// then resolves it via `vh_meta_update_field_index`, and the SECOND call
/// has to see the newly resolved slot. A memo that outlived that
/// transition would keep every such handle on the slow path forever — and
/// silently, because the answer would still be a legal `None`.
#[test]
fn a_generation_bump_discards_a_memoised_negative() {
    // A private VM identity (gc-common w18-b, w36-a).
    let vm = 0xB36_0001;
    let _forget = ForgetVm(vm);
    let addr = 0x5EED_1230usize;
    let ctx = ctx_for(vm);
    let key = ctx.identity_hash_code(at(addr));
    // The handle's weak lock key, minted but with no row under it yet.
    let lock_key = crate::gc_stable_weak_lock_key(&ctx, at(addr))
        .unwrap_or_else(|_| unreachable!("never fails"));
    // Nothing under this key yet: `None`, and now memoised as `None`.
    assert_eq!(
        varhandle_instance_field_plan_for_handle(vm, key, addr),
        None
    );
    {
        let mut t = vh_meta_table().lock();
        t.insert(
            vm,
            lock_key,
            Arc::new(VarHandleMeta {
                kind: VH_KIND_INSTANCE,
                class_name: String::new(),
                field_name: String::new(),
                field_desc: "I".to_string(),
                field_index: 7,
                class_id: 0,
                read_only: false,
                exact: None,
                ref_type_class_id: 0,
                holder_class_id: 0,
            }),
        );
    }
    // Deliberately NOT asserting that the memo is stale here. That would be
    // asserting the cache caches, which is not a correctness property, and
    // it is racy besides: the generation counter is global, so any sibling
    // test bumping it makes the "stale" step spuriously fail. The property
    // that matters is the one below — a memoised NEGATIVE becomes visible
    // once the table publishes the change.
    vh_meta_bump_generation();
    let plan = varhandle_instance_field_plan_for_handle(vm, key, addr)
        .expect("visible after the bump");
    assert_eq!(plan.field_index, 7);
    assert_eq!(plan.value_desc, b'I');
}

/// A repeated lookup returns the same answer the uncached path would, which
/// is the whole contract — the memo is an optimisation, never a different
/// answer.
#[test]
fn the_memo_agrees_with_the_uncached_lookup() {
    let vm = 0xB36_0002;
    let _forget = ForgetVm(vm);
    let addr = 0x0BAD_5A18usize;
    let key = file(
        vm,
        addr,
        VarHandleMeta {
            kind: VH_KIND_INSTANCE,
            class_name: String::new(),
            field_name: String::new(),
            field_desc: "Ljava/lang/String;".to_string(),
            field_index: 3,
            class_id: 0,
            read_only: false,
            exact: None,
            ref_type_class_id: 0,
            holder_class_id: 0,
        },
    );
    for _ in 0..4 {
        assert_eq!(
            varhandle_instance_field_plan_for_handle(vm, key, addr),
            existing_weak_lock_key_at(vm, key, addr)
                .and_then(varhandle_instance_field_plan_uncached)
        );
    }
    // A reference field collapses to `L`, as `varhandle_instance_field_plan_uncached` does.
    assert_eq!(
        varhandle_instance_field_plan_for_handle(vm, key, addr)
            .unwrap()
            .value_desc,
        b'L'
    );
}

/// Round 11 wave 3: the plan carries every fact the JIT's thin write/CAS
/// helpers need to decline what the native refuses — the read-only bit of
/// a `final` field's handle, the coordinate class, and the declared
/// reference type. Before, it carried only the slot and the kind, and the
/// helpers wrote final fields and wrong-class receivers.
#[test]
fn the_plan_carries_the_natives_refusal_facts() {
    // A private VM and a fake handle address (gc-common w18-b, w36-a).
    let vm = 0xB36_0003;
    let _forget = ForgetVm(vm);
    let addr = 0x0F1A_A118usize;
    let key = file(
        vm,
        addr,
        VarHandleMeta {
            kind: VH_KIND_INSTANCE,
            class_name: "Holder".to_string(),
            field_name: "next".to_string(),
            field_desc: "LNode;".to_string(),
            field_index: 2,
            class_id: 41,
            read_only: true,
            exact: None,
            ref_type_class_id: 77,
            holder_class_id: 0,
        },
    );
    let plan =
        varhandle_instance_field_plan_for_handle(vm, key, addr).expect("instance handle");
    assert!(plan.read_only);
    assert_eq!(plan.class_id, 41);
    assert!(!plan.ref_value_unchecked);
    assert_eq!(plan.ref_type_class_id, 77);

    // `Object` is the one reference field the native does not judge. An
    // array field IS judged (wave 4); a one-dimensional primitive one
    // carries the element type a fast path can check in one header read.
    for (desc, unchecked, element) in [
        ("Ljava/lang/Object;", true, None),
        ("[I", false, Some(ArrayElementType::Int)),
        ("[[I", false, None),
        ("[Ljava/lang/String;", false, None),
    ] {
        // A refile under the same key (and a generation bump).
        file(
            vm,
            addr,
            VarHandleMeta {
                kind: VH_KIND_INSTANCE,
                class_name: "Holder".to_string(),
                field_name: "f".to_string(),
                field_desc: desc.to_string(),
                field_index: 1,
                class_id: 41,
                read_only: false,
                exact: None,
                ref_type_class_id: 0,
                holder_class_id: 0,
            },
        );
        let plan =
            varhandle_instance_field_plan_for_handle(vm, key, addr).expect("instance handle");
        assert_eq!(plan.ref_value_unchecked, unchecked, "{desc}");
        assert_eq!(plan.ref_array_element, element, "{desc}");
        assert!(!plan.read_only, "{desc}");
    }
    assert_eq!(vh_ref_type_lookup_name("LNode;"), Some("Node"));
    assert_eq!(vh_ref_type_lookup_name("Ljava/lang/Object;"), None);
    assert_eq!(vh_ref_type_lookup_name("I"), None);
    assert_eq!(vh_ref_type_lookup_name("[LNode;"), None);
}

/// rt proposal W8-2: for a one-dimensional reference-array field the
/// meta's id is the COMPONENT's, and the plan exposes it only as
/// `ref_array_component_class_id`. It must never reach
/// `ref_type_class_id`, where the thin helpers' subtype test would admit a
/// plain `Node` into a `Node[]` field.
#[test]
fn a_reference_array_field_plan_carries_its_component_not_a_scalar_type() {
    assert_eq!(vh_ref_array_component_lookup_name("[LNode;"), Some("Node"));
    assert_eq!(
        vh_ref_array_component_lookup_name("[Ljava/lang/Object;"),
        Some("java/lang/Object")
    );
    assert_eq!(vh_ref_array_component_lookup_name("[[LNode;"), None);
    assert_eq!(vh_ref_array_component_lookup_name("[I"), None);
    assert_eq!(vh_ref_array_component_lookup_name("LNode;"), None);
    assert_eq!(vh_ref_array_component_lookup_name("[L;"), None);

    // A private VM and a fake handle address (gc-common w18-b, w36-a).
    let vm = 0xB36_0004;
    let _forget = ForgetVm(vm);
    let addr = 0x0A77_C0D8usize;
    for (desc, scalar, component) in [
        ("[LNode;", 0u32, 88u32),
        ("LNode;", 88, 0),
        // Never minted with an id; an id here must still not reach
        // either field.
        ("[[LNode;", 0, 0),
    ] {
        let key = file(
            vm,
            addr,
            VarHandleMeta {
                kind: VH_KIND_INSTANCE,
                class_name: "Holder".to_string(),
                field_name: "f".to_string(),
                field_desc: desc.to_string(),
                field_index: 1,
                class_id: 41,
                read_only: false,
                exact: None,
                ref_type_class_id: 88,
                holder_class_id: 0,
            },
        );
        let plan =
            varhandle_instance_field_plan_for_handle(vm, key, addr).expect("instance handle");
        assert_eq!(plan.ref_type_class_id, scalar, "{desc}");
        assert_eq!(plan.ref_array_component_class_id, component, "{desc}");
    }
}
}

/// gc-common w18-b (`common-w15e-varhandle-meta-tables-keyed-by-bare-identity-
/// hash`): the VarHandle meta rows and the FFM layout-handle shapes are keyed
/// by `(vm_identity, identity hash)`, answer only the VM that filed them, and
/// go with their VM at teardown.
///
/// Every test owns its VM identities (`0xB18_1xxx`). Mock contexts mint
/// disjoint addresses (and the mock's identity hash IS the address), so the
/// identity-hash collision two heaps produce naturally is modelled by ONE
/// mock object seen from two VM identities (`set_vm_identity` between the
/// calls): same hash, two VMs -- exactly the key pair the old table conflated.
#[cfg(test)]
mod w18b_var_handle_meta_per_vm_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn instance_meta(field_desc: &str, field_index: i32) -> VarHandleMeta {
    VarHandleMeta {
        kind: VH_KIND_INSTANCE,
        class_name: String::new(),
        field_name: String::new(),
        field_desc: field_desc.to_string(),
        field_index,
        class_id: 0,
        read_only: false,
        exact: None,
        ref_type_class_id: 0,
        holder_class_id: 0,
    }
}

/// Switch `ctx` to VM `vm` and hand it back, so a call reads as
/// `vh_meta_put(as_vm(&mut ctx, A), ..)`.
fn as_vm(ctx: &mut MockNativeContext, vm: usize) -> &mut MockNativeContext {
    ctx.set_vm_identity(vm);
    ctx
}

fn layout_shape(width: i32) -> SegmentVhShape {
    SegmentVhShape {
        width,
        carrier: b'I',
        little_endian: true,
        base_offset: 0,
        stride: 0,
        alignment: 1,
        target_size: 0,
        target_align: 1,
        index_bound: None,
        scaled_index: false,
        enclosing_size: 0,
    }
}

/// The page's failure scenario: VM A files `Foo.count` (slot 3) and VM B
/// files `Bar.name` (slot 1) under the same hash. B's put used to overwrite
/// A's row, and A then read slot 1 of a `Foo`.
#[test]
fn two_vms_colliding_on_one_identity_hash_each_read_their_own_meta() {
    let (vm_a, vm_b) = (0xB18_1001, 0xB18_1002);
    let mut ctx = MockNativeContext::new();
    let vh = ctx.fresh_object_ref();
    let hash = ctx.identity_hash_code(vh);
    vh_meta_put(as_vm(&mut ctx, vm_a), vh, instance_meta("I", 3));
    vh_meta_put(
        as_vm(&mut ctx, vm_b),
        vh,
        instance_meta("Ljava/lang/String;", 1),
    );

    let seen_a = vh_meta_get(as_vm(&mut ctx, vm_a), vh).expect("A's row");
    let seen_b = vh_meta_get(as_vm(&mut ctx, vm_b), vh).expect("B's row");
    assert_eq!((seen_a.field_index, seen_a.field_desc.as_str()), (3, "I"));
    assert_eq!(
        (seen_b.field_index, seen_b.field_desc.as_str()),
        (1, "Ljava/lang/String;")
    );
    // The JIT's plan agrees, per VM.
    let addr = vh.as_ptr() as usize;
    assert_eq!(
        varhandle_instance_field_plan_for_handle(vm_a, hash, addr)
            .map(|p| (p.field_index, p.value_desc)),
        Some((3, b'I'))
    );
    assert_eq!(
        varhandle_instance_field_plan_for_handle(vm_b, hash, addr)
            .map(|p| (p.field_index, p.value_desc)),
        Some((1, b'L'))
    );

    // Resolving one VM's slot must not touch the other's row.
    vh_meta_update_field_index(as_vm(&mut ctx, vm_b), vh, 5);
    assert_eq!(
        vh_meta_get(as_vm(&mut ctx, vm_a), vh).unwrap().field_index,
        3
    );
    assert_eq!(
        vh_meta_get(as_vm(&mut ctx, vm_b), vh).unwrap().field_index,
        5
    );

    forget_vm_var_handle_meta(vm_a);
    forget_vm_var_handle_meta(vm_b);
}

/// A VM that never filed a row for a hash gets `None` -- the slot path --
/// even when another VM filed one under that hash.
#[test]
fn a_row_filed_by_another_vm_is_refused() {
    let (vm_a, vm_c) = (0xB18_1011, 0xB18_1012);
    let mut ctx = MockNativeContext::new();
    let vh = ctx.fresh_object_ref();
    let hash = ctx.identity_hash_code(vh);
    vh_meta_put(as_vm(&mut ctx, vm_a), vh, instance_meta("J", 2));
    assert!(vh_meta_get(as_vm(&mut ctx, vm_c), vh).is_none());
    assert_eq!(
        varhandle_instance_field_plan_for_handle(vm_c, hash, vh.as_ptr() as usize),
        None
    );
    // `vh_meta_update_field_index` on the unregistered handle is a no-op,
    // not an overwrite of A's row.
    vh_meta_update_field_index(as_vm(&mut ctx, vm_c), vh, 9);
    assert!(vh_meta_get(as_vm(&mut ctx, vm_c), vh).is_none());
    assert_eq!(
        vh_meta_get(as_vm(&mut ctx, vm_a), vh).unwrap().field_index,
        2
    );
    forget_vm_var_handle_meta(vm_a);
}

/// Teardown drops the VM's rows (both tables), leaves every other VM's,
/// and is idempotent.
#[test]
fn teardown_drops_only_the_vms_rows() {
    let (vm_a, vm_b) = (0xB18_1021, 0xB18_1022);
    let mut ctx = MockNativeContext::new();
    let vh = ctx.fresh_object_ref();
    let hash = ctx.identity_hash_code(vh);
    vh_meta_put(as_vm(&mut ctx, vm_a), vh, instance_meta("I", 4));
    vh_meta_put(as_vm(&mut ctx, vm_b), vh, instance_meta("I", 6));
    register_p67_memory_segment_var_handle(as_vm(&mut ctx, vm_a), vh, layout_shape(4));
    register_p67_memory_segment_var_handle(as_vm(&mut ctx, vm_b), vh, layout_shape(8));
    assert_eq!(
        p67_memory_segment_var_handle_width(as_vm(&mut ctx, vm_a), vh),
        Some(4)
    );
    assert_eq!(
        p67_memory_segment_var_handle_width(as_vm(&mut ctx, vm_b), vh),
        Some(8)
    );

    forget_vm_var_handle_meta(vm_a);
    assert!(
        vh_meta_get(as_vm(&mut ctx, vm_a), vh).is_none(),
        "A's meta row is gone"
    );
    assert_eq!(
        varhandle_instance_field_plan_for_handle(vm_a, hash, vh.as_ptr() as usize),
        None
    );
    assert_eq!(
        p67_segment_vh_shape(as_vm(&mut ctx, vm_a), vh).map(|s| s.width),
        None
    );
    assert_eq!(
        vh_meta_get(as_vm(&mut ctx, vm_b), vh).unwrap().field_index,
        6
    );
    assert_eq!(
        p67_memory_segment_var_handle_width(as_vm(&mut ctx, vm_b), vh),
        Some(8)
    );
    {
        let t = vh_meta_table().lock();
        assert!(t.rows.values().all(|row| row.vm != vm_a));
    }
    // Idempotent.
    forget_vm_var_handle_meta(vm_a);
    forget_vm_var_handle_meta(vm_b);
    assert!(vh_meta_get(as_vm(&mut ctx, vm_b), vh).is_none());
    assert_eq!(
        p67_memory_segment_var_handle_width(as_vm(&mut ctx, vm_b), vh),
        None
    );
}

/// Two layout handles of two VMs colliding on one hash keep their own
/// shapes (the FFM twin of the first test).
#[test]
fn layout_shapes_of_two_vms_colliding_on_one_hash_stay_apart() {
    let (vm_a, vm_b) = (0xB18_1031, 0xB18_1032);
    let mut ctx = MockNativeContext::new();
    let vh = ctx.fresh_object_ref();
    register_p67_memory_segment_var_handle(as_vm(&mut ctx, vm_a), vh, layout_shape(2));
    register_p67_memory_segment_var_handle(as_vm(&mut ctx, vm_b), vh, layout_shape(8));
    assert_eq!(
        p67_memory_segment_var_handle_width(as_vm(&mut ctx, vm_a), vh),
        Some(2)
    );
    assert_eq!(
        p67_memory_segment_var_handle_width(as_vm(&mut ctx, vm_b), vh),
        Some(8)
    );
    forget_vm_var_handle_meta(vm_a);
    forget_vm_var_handle_meta(vm_b);
}

/// The table's own bookkeeping, on a private table so sibling tests' rows
/// cannot change the verdict: a refile keeps one row, `forget_vm` takes
/// exactly that VM's rows and is idempotent.
#[test]
fn the_table_files_refiles_and_forgets_per_vm() {
    let (vm_a, vm_b) = (0xB18_1041, 0xB18_1042);
    // Fabricated weak lock keys (private table: nothing else reads them).
    let (key_a, key_b) = (0xA_0000_0001usize, 0xB_0000_0001usize);
    let mut t = VhMetaTable::default();
    t.insert(vm_a, key_a, Arc::new(instance_meta("I", 3)));
    // Re-filing an existing row replaces it; it does not add one.
    t.insert(vm_a, key_a, Arc::new(instance_meta("I", 4)));
    assert_eq!(t.rows.len(), 1);
    t.insert(vm_b, key_b, Arc::new(instance_meta("J", 1)));
    assert_eq!(t.forget_vm(vm_b), 1);
    assert_eq!(t.get(key_a).unwrap().field_index, 4);
    assert!(t.get(key_b).is_none());
    assert_eq!(t.forget_vm(vm_b), 0, "idempotent");
    assert!(t.remove(key_a).is_some());
    assert!(t.rows.is_empty());
}

/// gc-common w29-b, w36-a: two live handles of ONE VM with one identity
/// hash (the mock hash is the address truncated to `i32`, so 4 GiB apart
/// -- lesson qq) each keep their own meta and their own JIT plan, and a
/// handle that was never registered is served neither a registered
/// collider's meta nor its plan -- the residual of
/// `common-w15e-varhandle-meta-tables-keyed-by-bare-identity-hash` (JIT
/// half) and `common-w33a-identity-hash-validated-memos-hit-for-a-colliding-replacement`
/// (site 5), where the hash-only lookup answered with the lone row.
#[test]
fn two_live_handles_of_one_vm_sharing_a_hash_keep_their_own_meta() {
    const VM: usize = 0xB29_1001;
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            forget_vm_var_handle_meta(VM);
            crate::forget_vm_lock_keys(VM);
        }
    }
    let _g = Forget;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    // SAFETY: keys only; the meta natives hash the address and never
    // dereference it (the mock's var-handle root is a no-op).
    let at = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
    let (a, b, stranger) = (at(0x1_B29A_1000), at(0x2_B29A_1000), at(0x3_B29A_1000));
    let hash = ctx.identity_hash_code(a);
    assert_eq!(
        hash,
        ctx.identity_hash_code(b),
        "premise: one identity hash"
    );
    assert_eq!(
        hash,
        ctx.identity_hash_code(stranger),
        "premise: one identity hash"
    );

    let plan = |h: ObjectRef| {
        varhandle_instance_field_plan_for_handle(VM, hash, h.as_ptr() as usize)
            .map(|p| (p.field_index, p.value_desc))
    };

    vh_meta_put(&mut ctx, a, instance_meta("I", 3));
    assert_eq!(plan(a), Some((3, b'I')));
    // THE residual: one registered row under the hash, and an
    // unregistered collider asking. The hash-only lookup served it A's
    // plan (slot 3 of whatever the stranger addresses).
    assert_eq!(
        plan(stranger),
        None,
        "an unregistered handle was served a lone registered collider's plan"
    );
    vh_meta_put(&mut ctx, b, instance_meta("Ljava/lang/String;", 1));
    let (ma, mb) = (
        vh_meta_get(&mut ctx, a).unwrap(),
        vh_meta_get(&mut ctx, b).unwrap(),
    );
    assert_eq!(
        (ma.field_index, ma.field_desc.as_str()),
        (3, "I"),
        "B overwrote A's meta"
    );
    assert_eq!(
        (mb.field_index, mb.field_desc.as_str()),
        (1, "Ljava/lang/String;")
    );
    assert!(
        vh_meta_get(&mut ctx, stranger).is_none(),
        "an unregistered handle was served a collider's meta"
    );
    // Two rows under the hash: each handle still gets its own plan (the
    // hash-only lookup declined both to the funnel).
    assert_eq!(plan(a), Some((3, b'I')));
    assert_eq!(plan(b), Some((1, b'L')));
    assert_eq!(plan(stranger), None);
    // Resolving B's slot leaves A's alone, and B's plan follows it.
    vh_meta_update_field_index(&mut ctx, b, 6);
    assert_eq!(vh_meta_get(&mut ctx, a).unwrap().field_index, 3);
    assert_eq!(vh_meta_get(&mut ctx, b).unwrap().field_index, 6);
    assert_eq!(plan(b), Some((6, b'L')));

    // B dies (its root was lost, say): its row goes with its key, and
    // neither B's address nor the stranger is served A's plan.
    assert_eq!(
        crate::gc_sweep_lock_keys(VM, &|x| x != b.as_ptr() as usize),
        1
    );
    assert!(vh_meta_get(&mut ctx, b).is_none());
    assert_eq!(vh_meta_get(&mut ctx, a).unwrap().field_index, 3);
    assert_eq!(plan(a), Some((3, b'I')));
    assert_eq!(plan(b), None);
    assert_eq!(plan(stranger), None);
}

/// gc-common w36-a: the JIT's plan names the VM too. A handle one VM
/// registered is not planned for another VM asking about the same address
/// and hash, and a VM with no rows at all does not make the other VM's
/// handles decline (the VM-less lookup declined every handle while two VMs
/// owned rows).
#[test]
fn the_jit_plan_of_one_vm_is_not_served_to_another() {
    const VM_A: usize = 0xB36_1001;
    const VM_B: usize = 0xB36_1002;
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            for vm in [VM_A, VM_B] {
                forget_vm_var_handle_meta(vm);
                crate::forget_vm_lock_keys(vm);
            }
        }
    }
    let _g = Forget;
    let mut ctx = MockNativeContext::new();
    // SAFETY: keys only, never dereferenced.
    let at = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
    let (ha, hb) = (0x1_B36C_1000usize, 0x2_B36C_1000usize);
    let hash = ctx.identity_hash_code(at(ha));
    assert_eq!(
        hash,
        ctx.identity_hash_code(at(hb)),
        "premise: one identity hash"
    );
    vh_meta_put(as_vm(&mut ctx, VM_A), at(ha), instance_meta("I", 3));
    assert_eq!(
        varhandle_instance_field_plan_for_handle(VM_B, hash, ha),
        None,
        "VM B was served VM A's plan"
    );
    vh_meta_put(as_vm(&mut ctx, VM_B), at(hb), instance_meta("J", 5));
    // Both VMs own rows: each still gets its own handle's plan.
    assert_eq!(
        varhandle_instance_field_plan_for_handle(VM_A, hash, ha).map(|p| p.field_index),
        Some(3)
    );
    assert_eq!(
        varhandle_instance_field_plan_for_handle(VM_B, hash, hb).map(|p| p.field_index),
        Some(5)
    );
    assert_eq!(
        varhandle_instance_field_plan_for_handle(VM_A, hash, hb),
        None
    );
    // A's teardown leaves B's plan alone.
    forget_vm_var_handle_meta(VM_A);
    crate::forget_vm_lock_keys(VM_A);
    assert_eq!(
        varhandle_instance_field_plan_for_handle(VM_A, hash, ha),
        None
    );
    assert_eq!(
        varhandle_instance_field_plan_for_handle(VM_B, hash, hb).map(|p| p.field_index),
        Some(5)
    );
}

/// gc-common w29-b: a moved handle (a hash-preserving move keeps the low
/// 32 bits) is still served its meta after the epilogue's lock-key remap,
/// and the stale address is not: the remap also retires the per-thread
/// memo line keyed by it.
#[test]
fn a_moved_handle_keeps_its_meta_and_its_old_address_does_not() {
    const VM: usize = 0xB29_1002;
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            forget_vm_var_handle_meta(VM);
            crate::forget_vm_lock_keys(VM);
        }
    }
    let _g = Forget;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    // SAFETY: keys only, never dereferenced.
    let at = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
    let (old, new) = (0x1_B29B_2000usize, 0x6_B29B_2000usize);
    vh_meta_put(&mut ctx, at(old), instance_meta("J", 2));
    register_p67_memory_segment_var_handle(&mut ctx, at(old), layout_shape(4));
    assert_eq!(vh_meta_get(&mut ctx, at(old)).unwrap().field_index, 2);
    assert_eq!(
        p67_memory_segment_var_handle_width(&mut ctx, at(old)),
        Some(4)
    );

    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM, &|x| x == old, &moved);
    assert_eq!(vh_meta_get(&mut ctx, at(new)).unwrap().field_index, 2);
    assert_eq!(
        p67_memory_segment_var_handle_width(&mut ctx, at(new)),
        Some(4)
    );
    assert!(
        vh_meta_get(&mut ctx, at(old)).is_none(),
        "the vacated address was still served the moved handle's meta"
    );
    assert_eq!(p67_memory_segment_var_handle_width(&mut ctx, at(old)), None);
    // The JIT's plan follows the handle to its new address, and its old
    // address (even with the hash it had) is planned for nothing.
    let hash = ctx.identity_hash_code(at(new));
    assert_eq!(
        varhandle_instance_field_plan_for_handle(VM, hash, new).map(|p| p.field_index),
        Some(2)
    );
    assert_eq!(
        varhandle_instance_field_plan_for_handle(VM, hash, old),
        None
    );
}
}

#[cfg(test)]
mod segment_heap_tests {
use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// -----------------------------------------------------------------------
// NOM F35-3 — `SegmentVarHandle` heap access must not assume `byte[]`.
//
// `segment_heap_get`/`segment_heap_set` used to read/write `base` one BYTE
// per `get_array_element`/`set_array_element` call regardless of its real
// element type, which is correct only for `byte[]`. A `VarHandle` bound to
// `MemorySegment.ofArray(new int[4])` read/wrote INT elements at BYTE
// granularity: `get_array_element(base, addr)` on an `int[]` reads whole
// element `addr`, not byte `addr` of element `addr / 4`. Wrong data with
// no crash, so nothing short of comparing against the oracle showed it.
// `heap_bytes_read`/`heap_bytes_write` (`panama.rs`, shared per that
// file's own doc comment) fix this by deriving the element width from
// `ctx.heap_element_type_of(base)` and addressing byte-within-element.
// -----------------------------------------------------------------------

/// A 4-byte write at byte offset 4 of an `int[4]` must land entirely in
/// element 1, leaving elements 0 and 2 untouched. Pre-fix, `addr=4` would
/// have been read as element INDEX 4 — out of bounds on a 4-element array
/// — and the write would have silently done nothing at all (the old bounds
/// check compared `addr` against the ELEMENT count, not the byte length).
#[test]
fn segment_heap_set_writes_a_whole_element_not_a_byte_index() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
    assert!(segment_heap_set(&mut ctx, arr, 4, 4, 0x1122_3344));
    assert_eq!(ctx.get_array_element(arr, 0), Value::Int(0));
    assert_eq!(
        ctx.get_array_element(arr, 1),
        Value::Int(0x1122_3344u32 as i32)
    );
    assert_eq!(ctx.get_array_element(arr, 2), Value::Int(0));
    assert_eq!(ctx.get_array_element(arr, 3), Value::Int(0));
}

/// Round-trip through `segment_heap_get`, and — the part the old
/// byte-indexed body could not do at all — a SUB-ELEMENT read: byte 1 of a
/// little-endian `int` is its second-lowest byte, not `get_array_element`
/// applied to index 1.
#[test]
fn segment_heap_get_reads_a_byte_within_an_element_little_endian() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
    ctx.set_array_element(arr, 0, Value::Int(0x0102_0304));
    // Pre-fix this called `get_array_element(arr, 1)` — a DIFFERENT
    // element, still zero here — and truncated it to a byte: 0, not 0x03.
    assert_eq!(segment_heap_get(&mut ctx, arr, 1, 1), Some(0x03));
    assert_eq!(segment_heap_get(&mut ctx, arr, 0, 4), Some(0x0102_0304));
}

/// The bounds check is in BYTES, matching the general reader: a 4-element
/// `int[]` is 16 bytes, so a 4-byte access at byte offset 16 is out of
/// range and must refuse rather than silently succeed against a
/// mismeasured (element-count) bound.
#[test]
fn segment_heap_access_bounds_check_is_in_bytes_not_elements() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
    assert_eq!(segment_heap_get(&mut ctx, arr, 12, 4), Some(0));
    assert_eq!(segment_heap_get(&mut ctx, arr, 16, 4), None);
    assert!(segment_heap_set(&mut ctx, arr, 12, 4, 7));
    assert!(!segment_heap_set(&mut ctx, arr, 16, 4, 7));
}

/// The pre-existing `byte[]` case is unchanged — the negative control that
/// keeps this fix from being a regression for the common case it replaced.
#[test]
fn segment_heap_access_still_works_for_a_plain_byte_array() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
    assert!(segment_heap_set(&mut ctx, arr, 0, 4, 0x0102_0304));
    // Little-endian: `ofArray(new byte[8]).set(JAVA_INT_UNALIGNED, 0,
    // 0x01020304)` leaves `[4, 3, 2, 1, 0, 0, 0, 0]` (F35-1 §4.3, measured).
    assert_eq!(ctx.get_array_element(arr, 0), Value::Int(4));
    assert_eq!(ctx.get_array_element(arr, 1), Value::Int(3));
    assert_eq!(ctx.get_array_element(arr, 2), Value::Int(2));
    assert_eq!(ctx.get_array_element(arr, 3), Value::Int(1));
    assert_eq!(segment_heap_get(&mut ctx, arr, 0, 4), Some(0x0102_0304));
}
}
