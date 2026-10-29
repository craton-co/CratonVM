// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

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

/// Round 11 wave 16, lane vh: the `asType` value adaptation for a PRIMITIVE
/// variable (`vh_adapt_value_args`), driven through the registered natives
/// with the value-site cell armed the way `vm_exec`'s dispatch arms it. Probe
/// twins: `R11W15RtVarHandleObjectSite`, `R11W16Vh*`.
#[cfg(test)]
mod vh_value_adapt_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::poly_call_site::{arm_vh_value_site, vh_site};
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    fn pack(prev: u8, last: u8) -> u16 {
        ((prev as u16) << 8) | last as u16
    }

    fn obj(ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
        match ctx.new_object(class).unwrap() {
            Some(Value::Object(Some(o))) => o,
            _ => unreachable!(),
        }
    }

    fn wrapper(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
        let o = obj(ctx, class);
        ctx.set_field(o, 0, payload);
        Value::Object(Some(o))
    }

    /// A handle over field 0 of a fresh `Box`, whose descriptor is `desc`.
    fn handle(ctx: &mut MockNativeContext, desc: &str) -> (ObjectRef, ObjectRef) {
        let recv = obj(ctx, "Box");
        let cid = ctx.class_id_of_object(recv).as_u32();
        let vh = obj(ctx, "java/lang/invoke/VarHandle");
        vh_meta_put(
            ctx,
            vh,
            VarHandleMeta {
                kind: VH_KIND_INSTANCE,
                class_name: "Box".to_string(),
                field_name: "f".to_string(),
                field_desc: desc.to_string(),
                field_index: 0,
                class_id: cid,
                read_only: false,
                exact: None,
                ref_type_class_id: 0,
                holder_class_id: 0,
            },
        );
        (vh, recv)
    }

    fn set(
        ctx: &mut MockNativeContext,
        vh: ObjectRef,
        recv: ObjectRef,
        v: Value,
        site: u16,
    ) -> MethodCallResult {
        arm_vh_value_site(site);
        varhandle_set(
            ctx,
            &[Value::Object(Some(vh)), Value::Object(Some(recv)), v],
        )
    }

    fn is_cce(err: &MethodCallFailed) -> bool {
        matches!(
            err,
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { .. }
            ))
        )
    }

    fn is_npe(err: &MethodCallFailed) -> bool {
        matches!(
            err,
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { .. }
            ))
        )
    }

    fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
        match err {
            MethodCallFailed::ExceptionThrown(o) => {
                ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                    == Some("java/lang/invoke/WrongMethodTypeException")
            }
            _ => false,
        }
    }

    /// The probe's `boxed` and `short` rows: an `Integer` / `Short` through an
    /// `Object` site stores its VALUE. Before: the wrapper's address.
    #[test]
    fn an_object_site_unboxes_and_widens_a_wrapper() {
        let mut ctx = MockNativeContext::new();
        let (vh, recv) = handle(&mut ctx, "I");
        let site = pack(vh_site::OTHER, vh_site::OBJECT);
        let five = wrapper(&mut ctx, "java/lang/Integer", Value::Int(5));
        set(&mut ctx, vh, recv, five, site).unwrap();
        assert_eq!(ctx.get_field(recv, 0), Value::Int(5));
        let seven = wrapper(&mut ctx, "java/lang/Short", Value::Int(7));
        set(&mut ctx, vh, recv, seven, site).unwrap();
        assert_eq!(ctx.get_field(recv, 0), Value::Int(7), "Short widens to int");
        let c = wrapper(&mut ctx, "java/lang/Character", Value::Int(0x41));
        set(&mut ctx, vh, recv, c, site).unwrap();
        assert_eq!(
            ctx.get_field(recv, 0),
            Value::Int(0x41),
            "Character widens to int"
        );
    }

    /// The probe's `long` and `objstr` rows (CCE through an `Object` site) and
    /// its `str` row (WMTE through a `String` site), plus null (NPE) and a
    /// `Long`-typed site (WMTE, refused statically). Nothing is stored.
    #[test]
    fn refusals_follow_the_call_site_type() {
        let mut ctx = MockNativeContext::new();
        let (vh, recv) = handle(&mut ctx, "I");
        ctx.set_field(recv, 0, Value::Int(3));
        let object_site = pack(vh_site::OTHER, vh_site::OBJECT);
        let long = wrapper(&mut ctx, "java/lang/Long", Value::Long(9));
        let err = set(&mut ctx, vh, recv, long, object_site).unwrap_err();
        assert!(is_cce(&err), "Long -> int through Object is CCE: {err:?}");
        let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
        let err = set(&mut ctx, vh, recv, s, object_site).unwrap_err();
        assert!(is_cce(&err), "String through Object is CCE: {err:?}");
        let err = set(&mut ctx, vh, recv, Value::Object(None), object_site).unwrap_err();
        assert!(is_npe(&err), "null through Object is NPE: {err:?}");
        let err = set(&mut ctx, vh, recv, s, pack(vh_site::OTHER, vh_site::OTHER)).unwrap_err();
        assert!(
            is_wmte(&ctx, &err),
            "String through String is WMTE: {err:?}"
        );
        let long_site = pack(vh_site::OTHER, vh_site::W_LONG);
        let err = set(&mut ctx, vh, recv, long, long_site).unwrap_err();
        assert!(is_wmte(&ctx, &err), "a Long-typed site is WMTE: {err:?}");
        let err = set(
            &mut ctx,
            vh,
            recv,
            Value::Object(None),
            pack(vh_site::OTHER, vh_site::OTHER),
        )
        .unwrap_err();
        assert!(
            is_wmte(&ctx, &err),
            "null through String is WMTE, not NPE: {err:?}"
        );
        assert_eq!(
            ctx.get_field(recv, 0),
            Value::Int(3),
            "no refused store landed"
        );
    }

    /// No door armed (the JIT's site-cached dispatch, a direct call): the
    /// run-time `Object`-site rules, so a wrapper still stores its value.
    #[test]
    fn an_unarmed_call_applies_the_object_site_rules() {
        let mut ctx = MockNativeContext::new();
        let (vh, recv) = handle(&mut ctx, "I");
        let five = wrapper(&mut ctx, "java/lang/Integer", Value::Int(5));
        set(&mut ctx, vh, recv, five, 0).unwrap();
        assert_eq!(ctx.get_field(recv, 0), Value::Int(5));
        let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
        assert!(is_cce(&set(&mut ctx, vh, recv, s, 0).unwrap_err()));
    }

    /// A PRIMITIVE site: `set(h, 5)` on a `float` field stores `5.0f` (the heap
    /// coercion used to reinterpret the `Int` as float BITS), and on a `byte`
    /// field it is a narrowing `asType` refuses (WMTE).
    #[test]
    fn a_primitive_site_widens_numerically_and_refuses_narrowing() {
        let mut ctx = MockNativeContext::new();
        let (vh, recv) = handle(&mut ctx, "F");
        set(
            &mut ctx,
            vh,
            recv,
            Value::Int(5),
            pack(vh_site::OTHER, b'I'),
        )
        .unwrap();
        assert_eq!(ctx.get_field(recv, 0), Value::Float(5.0));
        set(&mut ctx, vh, recv, Value::Int(6), 0).unwrap();
        assert_eq!(
            ctx.get_field(recv, 0),
            Value::Float(6.0),
            "unarmed Int is an int"
        );
        let (bvh, brecv) = handle(&mut ctx, "B");
        let int_site = pack(vh_site::OTHER, b'I');
        let err = set(&mut ctx, bvh, brecv, Value::Int(5), int_site).unwrap_err();
        assert!(is_wmte(&ctx, &err), "int -> byte narrows: {err:?}");
        set(
            &mut ctx,
            bvh,
            brecv,
            Value::Int(5),
            pack(vh_site::OTHER, b'B'),
        )
        .unwrap();
        assert_eq!(ctx.get_field(brecv, 0), Value::Int(5));
    }

    /// `compareAndSet(h, (Object) 5, (Object) 9)` on an `int` field: the
    /// EXPECTED value is unboxed too, so the CAS succeeds. Before, an
    /// `Integer` reference never matched the `int` slot.
    #[test]
    fn an_object_site_cas_unboxes_both_values() {
        let mut ctx = MockNativeContext::new();
        let (vh, recv) = handle(&mut ctx, "I");
        ctx.set_field(recv, 0, Value::Int(5));
        let expected = wrapper(&mut ctx, "java/lang/Integer", Value::Int(5));
        let new = wrapper(&mut ctx, "java/lang/Integer", Value::Int(9));
        arm_vh_value_site(pack(vh_site::OBJECT, vh_site::OBJECT));
        let ok = varhandle_compare_and_set(
            &mut ctx,
            &[
                Value::Object(Some(vh)),
                Value::Object(Some(recv)),
                expected,
                new,
            ],
        )
        .unwrap();
        assert_eq!(
            ok,
            Some(Value::Int(1)),
            "boxed expectation matches the int slot"
        );
        assert_eq!(ctx.get_field(recv, 0), Value::Int(9));
    }

    /// The cell is TAKEN: a native that runs leaves nothing for the next one.
    #[test]
    fn the_native_takes_the_value_site() {
        let mut ctx = MockNativeContext::new();
        let (vh, recv) = handle(&mut ctx, "I");
        set(
            &mut ctx,
            vh,
            recv,
            Value::Int(1),
            pack(vh_site::OTHER, b'I'),
        )
        .unwrap();
        assert_eq!(cratonvm_native_api::poly_call_site::take_vh_value_site(), 0);
    }

    /// Round 11 wave 17 (r11w16-vh-primitive-into-reference-variable-stores-null):
    /// `set(h, 100000)` on an `Object` field stores an `Integer` whose payload
    /// is the value, armed or not. Before: `coerce_field_value_for_slot` turned
    /// the raw `Int` into `null` at the reference slot. The values sit outside
    /// every wrapper cache, so no VM-scoped cache entry from another test's
    /// mock heap can answer.
    #[test]
    fn a_primitive_into_an_object_variable_is_boxed() {
        let mut ctx = MockNativeContext::new();
        let (vh, recv) = handle(&mut ctx, "Ljava/lang/Object;");
        set(
            &mut ctx,
            vh,
            recv,
            Value::Int(100_000),
            pack(vh_site::OTHER, b'I'),
        )
        .unwrap();
        let Value::Object(Some(b)) = ctx.get_field(recv, 0) else {
            panic!("the store must leave a box, not null");
        };
        assert_eq!(ctx.get_field(b, 0), Value::Int(100_000));
        // Unarmed: the variant decides (`Long` -> `Long`).
        set(&mut ctx, vh, recv, Value::Long(1 << 40), 0).unwrap();
        let Value::Object(Some(b)) = ctx.get_field(recv, 0) else {
            panic!("the unarmed store must leave a box, not null");
        };
        assert_eq!(ctx.get_field(b, 0), Value::Long(1 << 40));
    }

    /// nio21 wave 12, lane L3 (`r21-varhandle-instance-cas-does-not-cast-expected`):
    /// a wrong-typed EXPECTATION is the JDK body's `fieldType.cast(expected)`
    /// `ClassCastException` in `compareAndSet` and `compareAndExchange`, as a
    /// wrong-typed new value already was, and nothing is stored. Judged only
    /// where the mock can judge the new value's cast too (its class model is
    /// the same predicate's).
    #[test]
    fn a_wrong_typed_expectation_is_cce_in_both_cas_doors() {
        let mut ctx = MockNativeContext::new();
        let (vh, recv) = handle(&mut ctx, "Ljava/lang/Integer;");
        let keep = wrapper(&mut ctx, "java/lang/Integer", Value::Int(1));
        ctx.set_field(recv, 0, keep);
        let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
        let h = Value::Object(Some(vh));
        let r = Value::Object(Some(recv));
        let site = pack(vh_site::OBJECT, vh_site::OBJECT);
        arm_vh_value_site(site);
        let wrong_new = varhandle_compare_and_set(&mut ctx, &[h, r, keep, s]);
        if !matches!(&wrong_new, Err(e) if is_cce(e)) {
            return;
        }
        arm_vh_value_site(site);
        let err = varhandle_compare_and_set(&mut ctx, &[h, r, s, Value::Object(None)]).unwrap_err();
        assert!(
            is_cce(&err),
            "compareAndSet with a String expectation: {err:?}"
        );
        arm_vh_value_site(site);
        let err =
            varhandle_compare_and_exchange(&mut ctx, &[h, r, s, Value::Object(None)]).unwrap_err();
        assert!(
            is_cce(&err),
            "compareAndExchange with a String expectation: {err:?}"
        );
        assert_eq!(ctx.get_field(recv, 0), keep, "no refused CAS landed");
    }

    /// `asType`'s static refusal: `int` into a `String` field, `char` into a
    /// `Number` field and `int` into a `Long` field are WMTE, and nothing is
    /// stored.
    #[test]
    fn a_primitive_the_reference_variable_cannot_hold_is_wmte() {
        let mut ctx = MockNativeContext::new();
        let (svh, srecv) = handle(&mut ctx, "Ljava/lang/String;");
        ctx.set_field(srecv, 0, Value::Object(None));
        let err = set(
            &mut ctx,
            svh,
            srecv,
            Value::Int(5),
            pack(vh_site::OTHER, b'I'),
        )
        .unwrap_err();
        assert!(is_wmte(&ctx, &err), "int -> String is WMTE: {err:?}");
        let (nvh, nrecv) = handle(&mut ctx, "Ljava/lang/Number;");
        let err = set(
            &mut ctx,
            nvh,
            nrecv,
            Value::Int(0x41),
            pack(vh_site::OTHER, b'C'),
        )
        .unwrap_err();
        assert!(is_wmte(&ctx, &err), "char -> Number is WMTE: {err:?}");
        let (lvh, lrecv) = handle(&mut ctx, "Ljava/lang/Long;");
        let err = set(
            &mut ctx,
            lvh,
            lrecv,
            Value::Int(5),
            pack(vh_site::OTHER, b'I'),
        )
        .unwrap_err();
        assert!(is_wmte(&ctx, &err), "int -> Long is WMTE: {err:?}");
        assert_eq!(
            ctx.get_field(srecv, 0),
            Value::Object(None),
            "no refused store landed"
        );
    }

    /// The static table itself, beside `vh_site_admits_primitive`.
    #[test]
    fn the_reference_admits_primitive_table() {
        assert!(vh_reference_admits_primitive("Ljava/lang/Object;", b'Z'));
        assert!(vh_reference_admits_primitive(
            "Ljava/lang/Comparable;",
            b'C'
        ));
        assert!(vh_reference_admits_primitive("Ljava/lang/Number;", b'B'));
        assert!(!vh_reference_admits_primitive("Ljava/lang/Number;", b'Z'));
        assert!(vh_reference_admits_primitive(
            "Ljava/lang/constant/ConstantDesc;",
            b'D'
        ));
        assert!(!vh_reference_admits_primitive(
            "Ljava/lang/constant/ConstantDesc;",
            b'S'
        ));
        assert!(vh_reference_admits_primitive("Ljava/lang/Integer;", b'I'));
        assert!(!vh_reference_admits_primitive("Ljava/lang/Integer;", b'J'));
        assert!(!vh_reference_admits_primitive("[Ljava/lang/Object;", b'I'));
        assert!(!vh_reference_admits_primitive("Ljava/util/List;", b'I'));
    }

    /// `asType` checks every parameter before converting any: a null expected
    /// value through an `Object` site beside a `String`-typed new value is
    /// WMTE (the second value's static refusal), not the first value's NPE.
    #[test]
    fn a_static_refusal_of_either_value_wins_over_a_run_time_one() {
        let mut ctx = MockNativeContext::new();
        let (vh, recv) = handle(&mut ctx, "I");
        ctx.set_field(recv, 0, Value::Int(3));
        let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
        arm_vh_value_site(pack(vh_site::OBJECT, vh_site::OTHER));
        let err = varhandle_compare_and_set(
            &mut ctx,
            &[
                Value::Object(Some(vh)),
                Value::Object(Some(recv)),
                Value::Object(None),
                s,
            ],
        )
        .unwrap_err();
        assert!(
            is_wmte(&ctx, &err),
            "WMTE before the expected value's NPE: {err:?}"
        );
        assert_eq!(ctx.get_field(recv, 0), Value::Int(3));
    }

    /// A handle of any `kind` over field 0 of `Box`, for the arity tests.
    fn handle_of_kind(
        ctx: &mut MockNativeContext,
        kind: i32,
        class_name: &str,
        desc: &str,
    ) -> ObjectRef {
        let vh = obj(ctx, "java/lang/invoke/VarHandle");
        vh_meta_put(
            ctx,
            vh,
            VarHandleMeta {
                kind,
                class_name: class_name.to_string(),
                field_name: "f".to_string(),
                field_desc: desc.to_string(),
                field_index: 0,
                class_id: 0,
                read_only: false,
                exact: None,
                ref_type_class_id: 0,
                holder_class_id: 0,
            },
        );
        vh
    }

    /// Round 11 wave 19 (r11w18-orch-varhandle-set-accepts-a-surplus-reference-
    /// argument, probe `R11W18VhArrayValueArity`): `asType` refuses a call
    /// site whose argument COUNT differs from the access mode's, before any
    /// value is looked at. `A.set(h, arr, "s")` on an `int[]` field STORED
    /// `arr`; a missing value stored a default. Every access-mode family,
    /// surplus reference, surplus primitive and missing argument.
    #[test]
    fn a_surplus_or_missing_argument_is_wmte_in_every_access_mode_family() {
        let mut ctx = MockNativeContext::new();
        let (vh, recv) = handle(&mut ctx, "[I");
        let sentinel = ctx.new_array(ArrayElementType::Int, 1);
        let arr = ctx.new_array(ArrayElementType::Int, 3);
        ctx.set_field(recv, 0, Value::Object(Some(sentinel)));
        let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
        let h = Value::Object(Some(vh));
        let r = Value::Object(Some(recv));
        let a = Value::Object(Some(arr));
        let five = Value::Int(5);

        // set: surplus reference (the page's row), surplus int, missing value.
        for args in [vec![h, r, a, s], vec![h, r, a, five], vec![h, r]] {
            let err = varhandle_set(&mut ctx, &args).unwrap_err();
            assert!(is_wmte(&ctx, &err), "set {args:?}: {err:?}");
            let err = varhandle_set_volatile(&mut ctx, &args).unwrap_err();
            assert!(is_wmte(&ctx, &err), "setVolatile {args:?}: {err:?}");
        }
        assert_eq!(
            ctx.get_field(recv, 0),
            Value::Object(Some(sentinel)),
            "no refused store landed"
        );
        // get: a surplus argument.
        let err = varhandle_get(&mut ctx, &[h, r, s]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "get: {err:?}");
        // compareAndSet / compareAndExchange: surplus and missing.
        for args in [vec![h, r, a, a, s], vec![h, r, a]] {
            let err = varhandle_compare_and_set(&mut ctx, &args).unwrap_err();
            assert!(is_wmte(&ctx, &err), "compareAndSet {args:?}: {err:?}");
            let err = varhandle_compare_and_exchange(&mut ctx, &args).unwrap_err();
            assert!(is_wmte(&ctx, &err), "compareAndExchange {args:?}: {err:?}");
        }
        // getAndSet: surplus and missing.
        for args in [vec![h, r, a, s], vec![h, r]] {
            let err = varhandle_get_and_set(&mut ctx, &args).unwrap_err();
            assert!(is_wmte(&ctx, &err), "getAndSet {args:?}: {err:?}");
        }
        assert_eq!(ctx.get_field(recv, 0), Value::Object(Some(sentinel)));

        // The exact shape still stores.
        varhandle_set(&mut ctx, &[h, r, a]).unwrap();
        assert_eq!(ctx.get_field(recv, 0), Value::Object(Some(arr)));

        // getAndAdd / getAndBitwise* on an `int` field (an `int[]` field
        // refuses them with UOE first, which is HotSpot's order too).
        let (ivh, irecv) = handle(&mut ctx, "I");
        ctx.set_field(irecv, 0, Value::Int(10));
        let ih = Value::Object(Some(ivh));
        let ir = Value::Object(Some(irecv));
        for args in [vec![ih, ir, Value::Int(1), s], vec![ih, ir]] {
            let err = varhandle_get_and_add(&mut ctx, &args).unwrap_err();
            assert!(is_wmte(&ctx, &err), "getAndAdd {args:?}: {err:?}");
            let err = varhandle_get_and_bitwise_or(&mut ctx, &args).unwrap_err();
            assert!(is_wmte(&ctx, &err), "getAndBitwiseOr {args:?}: {err:?}");
        }
        assert_eq!(
            ctx.get_field(irecv, 0),
            Value::Int(10),
            "no refused update landed"
        );
    }

    /// The coordinate count comes from the handle's kind: none for a static
    /// field, two for an array element. A handle with no side-table entry is
    /// not judged (the count is not known).
    #[test]
    fn the_coordinate_count_follows_the_handle_kind() {
        let mut ctx = MockNativeContext::new();
        let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
        let stat = Value::Object(Some(handle_of_kind(&mut ctx, VH_KIND_STATIC, "Box", "I")));
        let err = varhandle_set(&mut ctx, &[stat, s, Value::Int(1)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "static set with a coordinate: {err:?}");
        let err = varhandle_get(&mut ctx, &[stat, s]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "static get with a coordinate: {err:?}");

        let av = Value::Object(Some(handle_of_kind(&mut ctx, VH_KIND_ARRAY, "[I", "I")));
        let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 2)));
        let err = varhandle_get(&mut ctx, &[av, arr, Value::Int(0), Value::Int(1)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "array get with a surplus: {err:?}");
        let err = varhandle_set(&mut ctx, &[av, arr, Value::Int(0)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "array set with no value: {err:?}");
        assert!(
            vh_check_arity(&mut ctx, &[av, arr, Value::Int(0), Value::Int(7)], 1, "set").is_ok()
        );
        // (A handle with no side-table entry is not judged. Not asserted here:
        // the side table is process-global and keyed by the identity hash,
        // which the mock takes from the address truncated to `i32`, so past
        // 4096 mock contexts in one test binary "no entry" is not a property
        // one test can own.)
    }
}

/// gc-common w15-e: the references `mh_dispatch` and `bindTo` hold across a GC
/// point (an allocation, or a first `<clinit>`) are rooted and re-read, and
/// every root is released on the way out.
///
/// The mock moves nothing on an allocation or a class initialisation (it has
/// only an `invoke_virtual` hook), so these tests pin down the observable
/// half of each fix: the values land where they must, and the pin stack is
/// balanced. Each test owns its VM identity (`0xE15_01xx`).
#[cfg(test)]
mod w15e_stale_reference_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    const OBJ_OBJ_DESC: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";

    fn vm_ctx(vm: usize) -> MockNativeContext {
        let ctx = MockNativeContext::new();
        ctx.set_vm_identity(vm);
        ctx
    }

    fn handle(v: MethodCallResult, what: &str) -> ObjectRef {
        match v {
            Ok(Some(Value::Object(Some(h)))) => h,
            other => panic!("{what} answered {other:?}"),
        }
    }

    fn identity(ctx: &mut MockNativeContext) -> ObjectRef {
        alloc_method_handle(
            ctx,
            "java/lang/invoke/MethodHandles",
            "identity",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            MH_KIND_IDENTITY,
        )
        .unwrap()
    }

    /// `factory.bindTo(a).bindTo(b)` on a lambda factory: the second bind
    /// allocates the capture array after reading the first bind's captures,
    /// `b` and the new handle. All three must survive into the new array, the
    /// first handle's array must be left alone, and no pin may leak.
    #[test]
    fn a_second_lambda_bind_keeps_both_captures_and_releases_its_pins() {
        let mut registry = NativeMethodRegistry::new();
        register_t4_method_handle_invoke(&mut registry);
        let bind = registry
            .find(
                "java/lang/invoke/MethodHandle",
                "bindTo",
                "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
            )
            .expect("MethodHandle.bindTo should be registered");
        let mut ctx = vm_ctx(0xE15_0101);
        let (a, b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
        let factory = alloc_method_handle(
            &mut ctx,
            "77",
            "",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Runnable;",
            MH_KIND_LAMBDA_FACTORY,
        )
        .unwrap();
        // No `type` MethodType: `bindTo`'s leading-parameter check then lets
        // the bind through without reading mock class mirrors.
        ctx.set_field_by_name(factory, "type", Value::Object(None));

        let once = handle(
            bind(
                &mut ctx,
                &[Value::Object(Some(factory)), Value::Object(Some(a))],
            ),
            "the first bindTo",
        );
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "the first bind releases its roots"
        );
        ctx.set_field_by_name(once, "type", Value::Object(None));
        let twice = handle(
            bind(
                &mut ctx,
                &[Value::Object(Some(once)), Value::Object(Some(b))],
            ),
            "the second bindTo",
        );
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "the second bind releases its roots"
        );

        let Value::Object(Some(captures)) = ctx.get_field(twice, MH_BOUND) else {
            panic!("the twice-bound factory must carry a capture array");
        };
        assert_eq!(ctx.array_length(captures), 2);
        assert_eq!(ctx.get_array_element(captures, 0), Value::Object(Some(a)));
        assert_eq!(ctx.get_array_element(captures, 1), Value::Object(Some(b)));
        let Value::Object(Some(first)) = ctx.get_field(once, MH_BOUND) else {
            panic!("the once-bound factory must carry a capture array");
        };
        assert_eq!(
            ctx.array_length(first),
            1,
            "the first handle's captures are not rewritten"
        );
        assert_eq!(ctx.get_array_element(first, 0), Value::Object(Some(a)));
    }

    /// A constructor handle roots `mh` and its arguments across the class's
    /// first `<clinit>` and releases every root (the arm's lowest pin moved
    /// from `new_obj` to `mh`).
    #[test]
    fn constructor_dispatch_releases_every_root() {
        let mut ctx = vm_ctx(0xE15_0102);
        let ctor = alloc_method_handle(
            &mut ctx,
            "java/lang/Object",
            "<init>",
            "(Ljava/lang/Object;)V",
            MH_KIND_CONSTRUCTOR,
        )
        .unwrap();
        let arg = ctx.fresh_object_ref();
        let made = mh_dispatch(&mut ctx, ctor, &[Value::Object(Some(arg))]).unwrap();
        assert!(
            matches!(made, Some(Value::Object(Some(_)))),
            "a constructor handle answers the new object, got {made:?}"
        );
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "every root must be released"
        );
    }

    /// `asCollector(Object[].class, 2)` over `identity`: both arguments are
    /// read through their roots after the collector array's allocation.
    #[test]
    fn a_collector_gathers_the_rooted_arguments() {
        let mut ctx = vm_ctx(0xE15_0103);
        let target = identity(&mut ctx);
        let wrapper = alloc_mh_carrier(&mut ctx, "__mh_collect_wrapper__", 3);
        ctx.set_field(wrapper, 0, Value::Object(Some(target)));
        ctx.set_field(wrapper, 1, Value::Int(2));
        ctx.set_field(wrapper, 2, Value::Object(None));
        let collector = alloc_method_handle(
            &mut ctx,
            "__adapter__",
            "collect",
            OBJ_OBJ_DESC,
            MH_KIND_COLLECT,
        )
        .unwrap();
        ctx.set_field(collector, MH_BOUND, Value::Object(Some(wrapper)));
        let (a, b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());

        let arr = handle(
            mh_dispatch(
                &mut ctx,
                collector,
                &[Value::Object(Some(a)), Value::Object(Some(b))],
            ),
            "the collector",
        );
        assert_eq!(ctx.array_length(arr), 2);
        assert_eq!(ctx.get_array_element(arr, 0), Value::Object(Some(a)));
        assert_eq!(ctx.get_array_element(arr, 1), Value::Object(Some(b)));
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "every root must be released"
        );
    }

    /// `spreadInvoker(type, 0)` SPREADS its trailing `Object[]` into the
    /// target's arguments (wave 26: it used to pack the arguments into a new
    /// array, the inverse of `invoker(type).asSpreader(..)`), with
    /// `asSpreader`'s length and null checks, and releases every root.
    #[test]
    fn a_spread_invoker_spreads_its_trailing_array() {
        let mut ctx = vm_ctx(0xE15_0104);
        let target = identity(&mut ctx);
        // `MH_DESC` is the TARGET descriptor (as `mh_alloc_invoker` records
        // it since wave 27), `(Object)Object`: one spread argument.
        let invoker = alloc_method_handle(
            &mut ctx,
            "__invoker__",
            "0",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            MH_KIND_INVOKER,
        )
        .unwrap();
        ctx.set_field_by_name(invoker, "type", Value::Object(None));
        let a = ctx.fresh_object_ref();
        let arr = ctx.new_array(ArrayElementType::Reference, 1);
        ctx.set_array_element(arr, 0, Value::Object(Some(a)));

        let spread = mh_dispatch(
            &mut ctx,
            invoker,
            &[Value::Object(Some(target)), Value::Object(Some(arr))],
        );
        assert_eq!(spread.unwrap(), Some(Value::Object(Some(a))));
        let short = ctx.new_array(ArrayElementType::Reference, 0);
        assert!(matches!(
            mh_dispatch(
                &mut ctx,
                invoker,
                &[Value::Object(Some(target)), Value::Object(Some(short))],
            ),
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::IllegalArgumentException { .. }
            )))
        ));
        assert!(matches!(
            mh_dispatch(
                &mut ctx,
                invoker,
                &[Value::Object(Some(target)), Value::Object(None)],
            ),
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { .. }
            )))
        ));
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "every root must be released"
        );
    }

    /// A reflective lambda factory stores the rooted captures (bound first,
    /// then direct) into the proxy it allocates.
    #[test]
    fn a_lambda_factory_stores_the_rooted_captures() {
        let mut ctx = vm_ctx(0xE15_0105);
        let (a, b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
        let factory = alloc_method_handle(
            &mut ctx,
            "77",
            "",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Runnable;",
            MH_KIND_LAMBDA_FACTORY,
        )
        .unwrap();
        let bound = ctx.new_array(ArrayElementType::Reference, 1);
        ctx.set_array_element(bound, 0, Value::Object(Some(a)));
        ctx.set_field(factory, MH_BOUND, Value::Object(Some(bound)));

        let proxy = handle(
            mh_dispatch(&mut ctx, factory, &[Value::Object(Some(b))]),
            "the lambda factory",
        );
        assert_eq!(ctx.get_field(proxy, 0), Value::Object(Some(a)));
        assert_eq!(ctx.get_field(proxy, 1), Value::Object(Some(b)));
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "every root must be released"
        );
    }
}

/// gc-common w19-c (stale-handle triage). The mock never moves an object, so
/// these check that the rooted rewrites keep their answers and release every
/// root; the move itself is not expressible here.
#[cfg(test)]
mod w19c_stale_handle_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    const MT_TO_MH: &str = "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;";

    fn vm_ctx(vm: usize) -> MockNativeContext {
        let ctx = MockNativeContext::new();
        ctx.set_vm_identity(vm);
        ctx
    }

    fn handle(v: MethodCallResult, what: &str) -> ObjectRef {
        match v {
            Ok(Some(Value::Object(Some(h)))) => h,
            other => panic!("{what} answered {other:?}"),
        }
    }

    /// Drops the VarHandle meta rows the test filed, on every exit path.
    struct ForgetVarHandleMeta(usize);

    impl Drop for ForgetVarHandleMeta {
        fn drop(&mut self) {
            forget_vm_var_handle_meta(self.0);
        }
    }

    /// `exactInvoker` / `invoker` / `spreadInvoker` stamp the CALLER's
    /// MethodType (read through its root, after `alloc_method_handle`), and
    /// `spreadInvoker` its count.
    #[test]
    fn the_invoker_factories_stamp_the_callers_type_and_release_their_roots() {
        let mut registry = NativeMethodRegistry::new();
        register_t28_method_handle_completeness(&mut registry);
        let mhs = "java/lang/invoke/MethodHandles";
        let mut ctx = vm_ctx(0xC19_0301);
        let mt = ctx.fresh_object_ref();
        for name in ["exactInvoker", "invoker"] {
            let factory = registry
                .find(mhs, name, MT_TO_MH)
                .unwrap_or_else(|| panic!("MethodHandles.{name} should be registered"));
            let mh = handle(factory(&mut ctx, &[Value::Object(Some(mt))]), name);
            assert_eq!(ctx.get_field_by_name(mh, "type"), Value::Object(Some(mt)));
            assert_eq!(ctx.get_field(mh, MH_KIND), Value::Int(MH_KIND_INVOKER));
            assert_eq!(
                ctx.native_pin_count_for_test(),
                0,
                "{name} releases its roots"
            );
        }
        let spread = registry
            .find(
                mhs,
                "spreadInvoker",
                "(Ljava/lang/invoke/MethodType;I)Ljava/lang/invoke/MethodHandle;",
            )
            .expect("MethodHandles.spreadInvoker should be registered");
        let mh = handle(
            spread(&mut ctx, &[Value::Object(Some(mt)), Value::Int(3)]),
            "spreadInvoker",
        );
        assert_eq!(ctx.get_field_by_name(mh, "type"), Value::Object(Some(mt)));
        let Value::Object(Some(count)) = ctx.get_field(mh, MH_NAME) else {
            panic!("spreadInvoker must record its count");
        };
        assert_eq!(ctx.read_string(count).as_deref(), Some("3"));
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }

    /// `CallSite.makeUninitializedCallSite(type)`: same shape, `args[1]`.
    #[test]
    fn an_uninitialized_call_site_handle_carries_the_callers_type() {
        let mut registry = NativeMethodRegistry::new();
        register_method_handle_combinator_extras_bridge(&mut registry);
        let make = registry
            .find(
                "java/lang/invoke/CallSite",
                "makeUninitializedCallSite",
                MT_TO_MH,
            )
            .expect("CallSite.makeUninitializedCallSite should be registered");
        let mut ctx = vm_ctx(0xC19_0302);
        let (cs, mt) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
        let mh = handle(
            make(
                &mut ctx,
                &[Value::Object(Some(cs)), Value::Object(Some(mt))],
            ),
            "makeUninitializedCallSite",
        );
        assert_eq!(ctx.get_field_by_name(mh, "type"), Value::Object(Some(mt)));
        assert_eq!(ctx.get_field(mh, MH_KIND), Value::Int(MH_KIND_CONSTANT));
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }

    /// Both VarHandle mints store their three names through the rooted
    /// `vh_store_names`.
    #[test]
    fn the_var_handle_mints_store_their_names_and_release_their_roots() {
        const VM: usize = 0xC19_0303;
        let _forget = ForgetVarHandleMeta(VM);
        let mut ctx = vm_ctx(VM);
        fn text(ctx: &MockNativeContext, vh: ObjectRef, slot: usize) -> Option<String> {
            match ctx.get_field(vh, slot) {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            }
        }

        let vh = alloc_instance_var_handle(&mut ctx, "a/B", "count", "I", 3, ClassId::new(7), None)
            .unwrap();
        assert_eq!(text(&ctx, vh, VH_CLASS).as_deref(), Some("a/B"));
        assert_eq!(text(&ctx, vh, VH_FIELD).as_deref(), Some("count"));
        assert_eq!(text(&ctx, vh, VH_FIELD_DESC).as_deref(), Some("I"));
        assert_eq!(ctx.get_field(vh, VH_FIELD_INDEX), Value::Int(3));
        assert_eq!(ctx.native_pin_count_for_test(), 0);

        let vh = alloc_static_var_handle(&mut ctx, "a/B", "TOTAL", "J", None).unwrap();
        assert_eq!(text(&ctx, vh, VH_CLASS).as_deref(), Some("a/B"));
        assert_eq!(text(&ctx, vh, VH_FIELD).as_deref(), Some("TOTAL"));
        assert_eq!(text(&ctx, vh, VH_FIELD_DESC).as_deref(), Some("J"));
        assert_eq!(ctx.get_field(vh, VH_FIELD_INDEX), Value::Int(-1));
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }
}

#[cfg(test)]
mod w20f_carrier_tests {
    //! gc-common w20-f: `alloc_mh_carrier_holding` stores every slot, the
    //! references at their current addresses, and releases what it roots.
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn w20f_carrier_holding_stores_each_slot_and_releases_its_roots() {
        let mut ctx = mock_ctx();
        let target = ctx.fresh_object_ref();
        let filter = ctx.fresh_object_ref();
        let pins_before = ctx.native_pin_count_for_test();
        let wrapper = alloc_mh_carrier_holding(
            &mut ctx,
            "__mh_w20f_test_wrapper__",
            &[
                Value::Object(Some(target)),
                Value::Object(None),
                Value::Int(7),
                Value::Object(Some(filter)),
            ],
        );
        assert_eq!(ctx.get_field(wrapper, 0), Value::Object(Some(target)));
        assert_eq!(ctx.get_field(wrapper, 1), Value::Object(None));
        assert_eq!(ctx.get_field(wrapper, 2), Value::Int(7));
        assert_eq!(ctx.get_field(wrapper, 3), Value::Object(Some(filter)));
        assert_eq!(
            ctx.native_pin_count_for_test(),
            pins_before,
            "no root left behind"
        );
    }
}

/// Interpreter round i1 wave 9, lane L6: the reflective `LambdaMetafactory`
/// path records the `Lookup`'s lookup class as the proxy's host, keys its
/// `CallSite` cache by it, and drops a host's rows when it unloads.
#[cfg(test)]
mod reflective_lambda_host_tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use cratonvm_native_api::NativeHeapAccess;

    #[test]
    fn the_host_is_the_lookup_class_of_args_0() {
        let mut ctx = mock_ctx();
        // The mock's reverse map reads a mirror's slot 0 as its class id.
        let mirror = ctx.alloc_object(ClassId::new(1), 1);
        ctx.set_field(mirror, 0, Value::Int(42));
        let lookup = ctx.alloc_object(ClassId::new(2), 3);
        ctx.set_field(lookup, 0, Value::Object(Some(mirror)));
        assert_eq!(
            reflective_lambda_host(&ctx, Some(&Value::Object(Some(lookup)))),
            Some(ClassId::new(42))
        );
        // No Lookup, a null Lookup, or a Lookup with no lookup class: no host.
        assert_eq!(reflective_lambda_host(&ctx, None), None);
        assert_eq!(
            reflective_lambda_host(&ctx, Some(&Value::Object(None))),
            None
        );
        let empty = ctx.alloc_object(ClassId::new(2), 3);
        ctx.set_field(empty, 0, Value::Object(None));
        assert_eq!(
            reflective_lambda_host(&ctx, Some(&Value::Object(Some(empty)))),
            None
        );
    }

    #[test]
    fn a_host_s_cached_call_sites_go_when_it_unloads_and_only_then() {
        let mut ctx = mock_ctx();
        let ccs = ctx.alloc_object(ClassId::new(3), 1);
        // The `impl_` key component: a real object's address (the scan turns
        // it back into an `ObjectRef`); the other components are 0.
        let impl_addr = ctx.alloc_object(ClassId::new(4), 0).as_ptr() as usize;
        // A VM identity no other test uses.
        let vm = usize::MAX - 0x196;
        let other_vm = vm - 1;
        let key = |host: u32| LambdaKey {
            host,
            invoked: 0,
            sam: 0,
            impl_: impl_addr,
            instantiated: 0,
        };
        {
            let mut cache = lambda_callsite_cache().lock();
            for host in [0u32, 7, 8] {
                cache.insert((vm, key(host)), ccs);
            }
            cache.insert((other_vm, key(7)), ccs);
        }
        forget_lambda_callsites_of_unloaded_hosts(vm, &|host| host == 7);
        {
            let cache = lambda_callsite_cache().lock();
            assert!(
                !cache.contains_key(&(vm, key(7))),
                "the unloaded host's row goes"
            );
            assert!(cache.contains_key(&(vm, key(8))), "a live host's row stays");
            assert!(cache.contains_key(&(vm, key(0))), "a host-less row stays");
            assert!(
                cache.contains_key(&(other_vm, key(7))),
                "another VM's row with a colliding host id stays"
            );
        }
        // The scan reports each row's values with its host as the owner.
        let mut seen: Vec<Option<u32>> = Vec::new();
        gc_scan_lambda_callsite_cache_roots(vm, &mut |owner, _| seen.push(owner));
        seen.sort_unstable();
        // One value per row (the `impl_` key component; the others are 0),
        // plus the `CallSite` itself.
        assert_eq!(seen, vec![None, None, Some(8), Some(8)]);
        forget_vm_lambda_callsite_cache(vm);
        forget_vm_lambda_callsite_cache(other_vm);
    }
}

/// Static field handles named through a subclass (interpreter round i1
/// wave 16, lane L3): they read and write the DECLARING class's static.
#[cfg(test)]
mod i16_l3_static_member_tests {
    use super::{
        alloc_method_handle, inherited_static_field_holder, mh_dispatch, static_field_holder,
        vh_static_slot, MH_KIND_GETTER, MH_KIND_SETTER,
    };
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::{FieldMetadata, NativeClassAccess, NativeSystemAccess};
    use cratonvm_types::{ClassId, Value};

    const ACC_STATIC: u16 = 0x0008;

    fn static_int(name: &str, slot: usize, declaring: ClassId) -> FieldMetadata {
        FieldMetadata {
            name: name.to_string(),
            descriptor: "I".to_string(),
            access_flags: ACC_STATIC,
            slot_index: slot,
            declaring_class_id: declaring,
            is_static: true,
        }
    }

    /// `p/I` (an interface) declares `k`, `p/P` declares `f` and `k`,
    /// `p/C extends P implements I` declares `own`.
    fn hierarchy(ctx: &mut MockNativeContext) -> (ClassId, ClassId, ClassId) {
        let i = ctx
            .ensure_class_initialized("p/I")
            .unwrap_or(ClassId::new(0));
        let p = ctx
            .ensure_class_initialized("p/P")
            .unwrap_or(ClassId::new(0));
        let c = ctx
            .ensure_class_initialized("p/C")
            .unwrap_or(ClassId::new(0));
        ctx.set_declared_fields(i, vec![static_int("k", 0, i)]);
        ctx.set_declared_fields(p, vec![static_int("f", 3, p), static_int("k", 4, p)]);
        ctx.set_declared_fields(c, vec![static_int("own", 1, c)]);
        ctx.set_superclass(c, p);
        ctx.set_interfaces(c, vec![i]);
        (i, p, c)
    }

    /// JVMS 5.4.3.2 order: the class itself, then superinterfaces, then the
    /// superclass; a field the named class declares is not "inherited".
    #[test]
    fn the_holder_follows_the_field_lookup_order() {
        let mut ctx = MockNativeContext::new();
        let (i, p, c) = hierarchy(&mut ctx);
        assert_eq!(static_field_holder(&ctx, c, "own"), Some(c));
        assert_eq!(static_field_holder(&ctx, c, "f"), Some(p));
        assert_eq!(
            static_field_holder(&ctx, c, "k"),
            Some(i),
            "interfaces first"
        );
        assert_eq!(static_field_holder(&ctx, c, "absent"), None);
        assert_eq!(inherited_static_field_holder(&ctx, c, "own"), None);
        assert_eq!(inherited_static_field_holder(&ctx, c, "f"), Some(p));
        assert_eq!(inherited_static_field_holder(&ctx, p, "f"), None);
        assert_eq!(inherited_static_field_holder(&ctx, c, "absent"), None);
    }

    /// `findStaticGetter(C.class, "f", int.class).invoke()` for an `f` that
    /// `P` declares reads `P.f`; the setter writes it. The arms read slot 0
    /// of `C` and wrote nothing before wave 16.
    #[test]
    fn a_static_field_handle_named_through_a_subclass_reaches_the_declaring_class() {
        let mut ctx = MockNativeContext::new();
        let (_, p, c) = hierarchy(&mut ctx);
        ctx.set_static_field(p, 3, Value::Int(42));
        ctx.set_static_field(c, 0, Value::Int(-1));
        let getter = alloc_method_handle(&mut ctx, "p/C", "f", "()I", MH_KIND_GETTER);
        let getter = getter.unwrap_or_else(|e| panic!("getter: {e:?}"));
        let read = mh_dispatch(&mut ctx, getter, &[]);
        assert!(
            matches!(read, Ok(Some(Value::Int(42)))),
            "C.f reads P.f: {read:?}"
        );

        let setter = alloc_method_handle(&mut ctx, "p/C", "f", "(I)V", MH_KIND_SETTER);
        let setter = setter.unwrap_or_else(|e| panic!("setter: {e:?}"));
        let wrote = mh_dispatch(&mut ctx, setter, &[Value::Int(7)]);
        assert!(wrote.is_ok(), "{wrote:?}");
        assert_eq!(ctx.get_static_field(p, 3), Value::Int(7));
        assert_eq!(
            ctx.get_static_field(c, 0),
            Value::Int(-1),
            "C's slot 0 untouched"
        );
    }

    /// The static VarHandle slot of an inherited field is the declaring
    /// class's; a field the named class declares keeps the named class.
    #[test]
    fn a_static_var_handle_slot_is_the_declaring_classes() {
        let mut ctx = MockNativeContext::new();
        let (_, p, c) = hierarchy(&mut ctx);
        let slot = |ctx: &mut MockNativeContext, field: &str, owner: Option<ClassId>| {
            vh_static_slot(
                ctx,
                "p/C",
                field,
                owner,
                &mut [],
                super::VhStaticInit {
                    mode: "",
                    variable: 0,
                },
            )
            .unwrap_or(None)
        };
        assert_eq!(slot(&mut ctx, "f", None), Some((p, 3)));
        assert_eq!(slot(&mut ctx, "f", Some(c)), Some((p, 3)));
        assert_eq!(slot(&mut ctx, "own", None), Some((c, 1)));
    }
}

#[cfg(test)]
mod i17_l4_lookup_init_tests {
    use super::{
        alloc_method_handle, lookup_find_constructor, lookup_find_getter, lookup_find_setter,
        lookup_find_special, lookup_find_static_getter, lookup_find_static_setter,
        lookup_find_static_var_handle, lookup_find_var_handle, lookup_find_virtual,
        arm_entry_call_site, lookup_load_class, mh_dispatch, vh_static_slot, MH_BOUND,
        MH_KIND_CONSTRUCTOR,
        MH_KIND_GETTER, MH_KIND_SETTER,
    };
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeHeapAccess, NativeSystemAccess,
    };
    use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};
    use cratonvm_types::{ClassId, Value};

    /// `findStaticGetter` / `findStaticSetter` / `findVarHandle` /
    /// `findStaticVarHandle` with a null `refc`, `name` or `type` throw
    /// `NullPointerException` (HotSpot's `resolveOrFail`), like the other
    /// finders; they handed back an inert handle or a null VarHandle before
    /// wave 17.
    #[test]
    fn the_static_and_var_handle_finders_refuse_a_null_argument() {
        let mut ctx = MockNativeContext::new();
        let cid = ctx
            .ensure_class_initialized("p/Target")
            .unwrap_or(ClassId::new(0));
        let mirror = Value::Object(Some(ctx.get_class_mirror(cid)));
        let name = Value::Object(Some(ctx.create_string("f")));
        let null = Value::Object(None);
        type Finder = fn(
            &mut dyn cratonvm_native_api::NativeContext,
            &[Value],
        ) -> cratonvm_types::error::MethodCallResult;
        let finders: [(&str, Finder); 4] = [
            ("findStaticGetter", lookup_find_static_getter),
            ("findStaticSetter", lookup_find_static_setter),
            ("findVarHandle", lookup_find_var_handle),
            ("findStaticVarHandle", lookup_find_static_var_handle),
        ];
        for (what, finder) in finders {
            for args in [
                [null, null, name, mirror],
                [null, mirror, null, mirror],
                [null, mirror, name, null],
            ] {
                let r = finder(&mut ctx, &args);
                assert!(
                    matches!(
                        r,
                        Err(MethodCallFailed::InternalError(VmError::Runtime(
                            RuntimeError::NullPointerException { .. }
                        )))
                    ),
                    "{what}: {r:?}"
                );
            }
        }
    }

    const ACC_STATIC: u16 = 0x0008;

    /// `p/S` with one static `int f` in slot 0.
    fn class_with_static(ctx: &mut MockNativeContext) -> ClassId {
        let s = ctx
            .ensure_class_initialized("p/S")
            .unwrap_or(ClassId::new(0));
        ctx.set_declared_fields(
            s,
            vec![FieldMetadata {
                name: "f".to_string(),
                descriptor: "I".to_string(),
                access_flags: ACC_STATIC,
                slot_index: 0,
                declaring_class_id: s,
                is_static: true,
            }],
        );
        s
    }

    /// HotSpot's `find*` run no `<clinit>`: the finders LOAD the class and
    /// leave initialization to the handle's first call. Whatever each finder
    /// answers here (the mock declares no members), it must not have asked
    /// to initialize the class it was handed.
    #[test]
    fn the_finders_load_the_class_without_initializing_it() {
        let mut ctx = MockNativeContext::new();
        let cid = ctx
            .ensure_class_initialized("p/Target")
            .unwrap_or(ClassId::new(0));
        let mirror = Value::Object(Some(ctx.get_class_mirror(cid)));
        let name = Value::Object(Some(ctx.create_string("m")));
        let mt = Value::Object(Some(ctx.alloc_object(ClassId::new(0), 2)));
        let before = ctx.class_init_requests().len();
        // A null Lookup receiver skips the access gate (`lk_enforce_find_access`).
        let lk = Value::Object(None);
        let _ = lookup_find_virtual(&mut ctx, &[lk, mirror, name, mt]);
        let _ = lookup_find_special(&mut ctx, &[lk, mirror, name, mt, mirror]);
        let _ = lookup_find_constructor(&mut ctx, &[lk, mirror, mt]);
        let _ = lookup_find_getter(&mut ctx, &[lk, mirror, name, mirror]);
        let _ = lookup_find_setter(&mut ctx, &[lk, mirror, name, mirror]);
        let loaded = lookup_load_class(&mut ctx, "p/Target");
        assert!(matches!(loaded, Ok(Some(id)) if id == cid), "{loaded:?}");
        let requests = ctx.class_init_requests();
        assert!(
            !requests[before..].iter().any(|n| n == "p/Target"),
            "a finder initialized its class: {requests:?}"
        );
    }

    /// A static getter / setter handle on a class whose `<clinit>` failed
    /// throws, as `getstatic` / `putstatic` do; it answered `0` and dropped
    /// the write before wave 17.
    #[test]
    fn a_static_field_handle_propagates_the_initialization_error() {
        let mut ctx = MockNativeContext::new();
        let s = class_with_static(&mut ctx);
        ctx.set_static_field(s, 0, Value::Int(5));
        ctx.fail_class_init("p/S");

        let getter = alloc_method_handle(&mut ctx, "p/S", "f", "()I", MH_KIND_GETTER);
        let getter = getter.unwrap_or_else(|e| panic!("getter: {e:?}"));
        let read = mh_dispatch(&mut ctx, getter, &[]);
        assert!(read.is_err(), "the getter must throw: {read:?}");

        let setter = alloc_method_handle(&mut ctx, "p/S", "f", "(I)V", MH_KIND_SETTER);
        let setter = setter.unwrap_or_else(|e| panic!("setter: {e:?}"));
        let wrote = mh_dispatch(&mut ctx, setter, &[Value::Int(7)]);
        assert!(wrote.is_err(), "the setter must throw: {wrote:?}");
        assert_eq!(ctx.get_static_field(s, 0), Value::Int(5), "no write lands");
    }

    /// A constructor handle allocates before adapting its entry arguments.
    /// If that adaptation refuses the call, it must withdraw the allocation's
    /// finalizer registration before releasing the pinned object.
    #[test]
    fn a_constructor_handle_withdraws_when_entry_adaptation_fails() {
        let mut ctx = MockNativeContext::new();
        let cid = ctx
            .ensure_class_initialized("p/F")
            .unwrap_or(ClassId::new(0));
        let ctor = alloc_method_handle(&mut ctx, "p/F", "<init>", "(I)V", MH_KIND_CONSTRUCTOR)
            .unwrap_or_else(|e| panic!("ctor: {e:?}"));
        ctx.set_field(ctor, MH_BOUND, Value::Int(cid.as_u32() as i32));
        arm_entry_call_site("(I)V");
        let rejected = mh_dispatch(&mut ctx, ctor, &[]);
        assert!(rejected.is_err(), "wrong arity must fail: {rejected:?}");
        assert_eq!(
            ctx.withdraw_finalizable_before_constructor_calls.borrow().len(),
            1,
            "the abandoned allocation's finalizer row is withdrawn"
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0, "pins are released");
    }

    /// The constructor arm throws the initialization error instead of
    /// answering `null`.
    #[test]
    fn a_constructor_handle_propagates_the_initialization_error() {
        let mut ctx = MockNativeContext::new();
        let s = class_with_static(&mut ctx);
        ctx.fail_class_init("p/S");
        let ctor = alloc_method_handle(&mut ctx, "p/S", "<init>", "()V", MH_KIND_CONSTRUCTOR);
        let ctor = ctor.unwrap_or_else(|e| panic!("ctor: {e:?}"));
        ctx.set_field(ctor, MH_BOUND, Value::Int(s.as_u32() as i32));
        let made = mh_dispatch(&mut ctx, ctor, &[]);
        assert!(made.is_err(), "the constructor must throw: {made:?}");
    }

    /// `vh_static_slot` propagates the error on both the owner and the
    /// by-name route, and hands the operands back unchanged when the holder
    /// initializes.
    #[test]
    fn a_static_var_handle_slot_propagates_the_initialization_error() {
        let mut ctx = MockNativeContext::new();
        let s = class_with_static(&mut ctx);
        let mut live = [Value::Int(9)];
        let ok = vh_static_slot(
            &mut ctx,
            "p/S",
            "f",
            Some(s),
            &mut live,
            super::VhStaticInit {
                mode: "",
                variable: 0,
            },
        );
        assert!(matches!(ok, Ok(Some((c, 0))) if c == s), "{ok:?}");
        assert_eq!(live[0], Value::Int(9));

        ctx.fail_class_init("p/S");
        let by_owner = vh_static_slot(
            &mut ctx,
            "p/S",
            "f",
            Some(s),
            &mut live,
            super::VhStaticInit {
                mode: "",
                variable: 0,
            },
        );
        assert!(by_owner.is_err(), "{by_owner:?}");
        let by_name = vh_static_slot(
            &mut ctx,
            "p/S",
            "f",
            None,
            &mut live,
            super::VhStaticInit {
                mode: "",
                variable: 0,
            },
        );
        assert!(by_name.is_err(), "{by_name:?}");
    }
}

#[cfg(test)]
mod w33b_static_lock_pin_order_tests {
    use super::vh_with_static_lock;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
    use cratonvm_types::{ClassId, ObjectRef, Value};

    /// The current depth of the mock's pin stack (the index the next pin gets).
    fn pin_depth(ctx: &mut MockNativeContext, probe: ObjectRef) -> usize {
        let depth = ctx.pin_native_root(probe);
        ctx.unpin_native_roots(depth);
        depth
    }

    /// gc-common w33-b: `vh_with_static_lock` pins the caller's operands
    /// BEFORE `get_class_mirror`, which mints the mirror on first use and so
    /// can collect. The mock cannot move an object, so the test pins the
    /// ORDER: the first pin the call takes is the first reference operand,
    /// not the mirror. It also checks that the body receives the operands
    /// (primitives untouched) and that the call releases every pin it took,
    /// with and without reference operands.
    #[test]
    fn w33b_operands_are_pinned_before_the_mirror_is_fetched() {
        let mut ctx = MockNativeContext::new();
        let witness = ctx.alloc_object(ClassId::new(0), 1);
        let stored = ctx.alloc_object(ClassId::new(0), 1);
        let before = pin_depth(&mut ctx, witness);
        let operands = [
            Value::Object(Some(witness)),
            Value::Int(3),
            Value::Object(Some(stored)),
        ];
        let (first_pinned, seen) =
            vh_with_static_lock(&mut ctx, ClassId::new(7), &operands, |ctx, live| {
                (ctx.read_native_pin(before, stored), live.to_vec())
            });
        assert_eq!(
            first_pinned, witness,
            "the first pin must be the first reference operand, not the mirror"
        );
        assert_eq!(seen, operands.to_vec());
        assert_eq!(pin_depth(&mut ctx, witness), before, "every pin released");

        let answer = vh_with_static_lock(&mut ctx, ClassId::new(7), &[], |_, live| live.len());
        assert_eq!(answer, 0);
        assert_eq!(
            pin_depth(&mut ctx, witness),
            before,
            "the mirror's pin is released when there is no operand"
        );
    }
}

#[cfg(test)]
mod r12w3_hfix_lookup_inherited_tests {
    //! Round 12 wave 3 (hfix): `findVirtual` / `findSpecial` /
    //! `MethodHandleNatives.resolve` resolve an instance method a class
    //! inherits from a superinterface (JVMS §5.4.3.3), `findStatic` does not.
    use super::{lk_superinterface_declares_instance_method, lookup_require_method};
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::{MethodMetadata, NativeClassAccess};
    use cratonvm_types::ClassId;

    fn class(ctx: &mut MockNativeContext, name: &str) -> ClassId {
        ctx.ensure_class_initialized(name)
            .unwrap_or(ClassId::new(0))
    }

    fn method(name: &str, desc: &str, access_flags: u16, owner: ClassId) -> MethodMetadata {
        MethodMetadata {
            name: name.to_string(),
            descriptor: desc.to_string(),
            access_flags,
            declaring_class_id: owner,
            exceptions: Vec::new(),
            signature: None,
        }
    }

    #[test]
    fn an_inherited_interface_method_resolves_for_the_instance_finders_only() {
        let mut ctx = MockNativeContext::new();
        // Greeter <- Loud (sub-interface) <- Animal implements Loud <- Dog.
        let greeter = class(&mut ctx, "p/Greeter");
        let loud = class(&mut ctx, "p/Loud");
        let animal = class(&mut ctx, "p/Animal");
        let dog = class(&mut ctx, "p/Dog");
        ctx.set_interfaces(loud, vec![greeter]);
        ctx.set_interfaces(animal, vec![loud]);
        ctx.set_superclass(dog, animal);
        // A public default, a public abstract, a private and a public static.
        ctx.set_declared_methods(
            greeter,
            vec![
                method("greet", "(I)I", 0x0001, greeter),
                method("shout", "()V", 0x0401, greeter),
                method("helper", "(I)I", 0x0002, greeter),
                method("util", "(I)I", 0x0009, greeter),
            ],
        );
        let found = |cls: &str, name: &str, desc: &str| {
            lk_superinterface_declares_instance_method(&ctx, cls, name, desc)
        };
        // A default the class inherits, directly and through a superclass.
        assert!(found("p/Animal", "greet", "(I)I"));
        assert!(found("p/Dog", "greet", "(I)I"));
        // A superinterface's method, asked on an interface.
        assert!(found("p/Loud", "greet", "(I)I"));
        // An abstract declaration resolves too.
        assert!(found("p/Dog", "shout", "()V"));
        // The descriptor must match; private and static are not inherited.
        assert!(!found("p/Dog", "greet", "(J)I"));
        assert!(!found("p/Dog", "helper", "(I)I"));
        assert!(!found("p/Dog", "util", "(I)I"));
        // A class's own declarations are `method_exists`'s answer, not this.
        assert!(!found("p/Greeter", "greet", "(I)I"));
        assert!(!found("p/Missing", "greet", "(I)I"));

        // The mock's `method_exists` answers false for everything, so only the
        // superinterface step can admit.
        let require = |loaded: bool, name: &str, desc: &str, instance: bool| {
            let ref_kind = if instance {
                "invokeVirtual"
            } else {
                "invokeStatic"
            };
            lookup_require_method(&ctx, loaded, "p/Dog", name, desc, ref_kind, instance)
        };
        assert!(require(true, "greet", "(I)I", true).is_ok());
        assert!(require(true, "greet", "(I)I", false).is_err());
        assert!(require(true, "util", "(I)I", true).is_err());
        assert!(require(false, "nothing", "()V", true).is_ok());
    }

    #[test]
    fn a_cyclic_interface_graph_terminates() {
        let mut ctx = MockNativeContext::new();
        let a = class(&mut ctx, "p/A");
        let b = class(&mut ctx, "p/B");
        let c = class(&mut ctx, "p/C");
        ctx.set_interfaces(a, vec![b]);
        ctx.set_interfaces(b, vec![a]);
        ctx.set_interfaces(c, vec![a]);
        ctx.set_superclass(c, c);
        assert!(!lk_superinterface_declares_instance_method(
            &ctx, "p/C", "m", "()V"
        ));
    }

    /// Interpreter round i3 wave 83: the holder `MethodHandleNatives.resolve`
    /// writes for a method member (HotSpot's `init_method_MemberName`); the
    /// end-to-end rows are `tools/probes/interp/L4/L4W83RevealInheritedMethods.java`.
    #[test]
    fn a_method_resolves_to_its_declaring_class() {
        use super::declaring_class_id;
        let mut ctx = MockNativeContext::new();
        let object = class(&mut ctx, "java/lang/Object");
        let base = class(&mut ctx, "p/Base");
        let sub = class(&mut ctx, "p/Sub");
        let leaf = class(&mut ctx, "p/Leaf");
        let super_i = class(&mut ctx, "p/SuperI");
        let sub_i = class(&mut ctx, "p/SubI");
        let d = class(&mut ctx, "p/D");
        let impl_ = class(&mut ctx, "p/Impl");
        ctx.set_superclass(base, object);
        ctx.set_superclass(sub, base);
        ctx.set_superclass(leaf, sub);
        ctx.set_superclass(impl_, object);
        for iface in [super_i, sub_i, d] {
            ctx.set_is_interface(iface, true);
        }
        ctx.set_interfaces(sub_i, vec![super_i]);
        ctx.set_interfaces(impl_, vec![d]);
        ctx.set_declared_methods(
            object,
            vec![
                method("hashCode", "()I", 0x0101, object),
                method("clone", "()Ljava/lang/Object;", 0x0104, object),
            ],
        );
        ctx.set_declared_methods(
            base,
            vec![
                method("s", "()I", 0x0009, base),
                method("m", "()I", 0x0001, base),
            ],
        );
        ctx.set_declared_methods(sub, vec![method("own", "()I", 0x0009, sub)]);
        ctx.set_declared_methods(
            super_i,
            vec![
                method("si", "()I", 0x0401, super_i),
                method("st", "()I", 0x0009, super_i),
            ],
        );
        ctx.set_declared_methods(d, vec![method("d", "()I", 0x0001, d)]);
        let holder =
            |start, name: &str, desc: &str| declaring_class_id(&ctx, start, name, desc, false);

        // Declared by the named class: no holder to write.
        assert_eq!(holder(sub, "own", "()I"), None);
        assert_eq!(holder(base, "m", "()I"), None);
        // Inherited from a superclass, one or two levels down; the descriptor must match.
        assert_eq!(holder(sub, "s", "()I"), Some(base));
        assert_eq!(holder(leaf, "m", "()I"), Some(base));
        assert_eq!(holder(leaf, "hashCode", "()I"), Some(object));
        assert_eq!(holder(leaf, "m", "()J"), None);
        // Through an interface: its superinterface's instance method, then
        // `Object`'s public ones -- not a static, nor a protected `clone`.
        assert_eq!(holder(sub_i, "si", "()I"), Some(super_i));
        assert_eq!(holder(sub_i, "st", "()I"), None);
        assert_eq!(holder(sub_i, "hashCode", "()I"), Some(object));
        assert_eq!(holder(sub_i, "clone", "()Ljava/lang/Object;"), None);
        // A default method a class inherits stays the class's.
        assert_eq!(holder(impl_, "d", "()I"), None);
    }
}

#[cfg(test)]
mod r12w4_find_special_receiver_tests {
    //! Round 12 wave 4 (hunter2): `findSpecial(refc, name, type, specialCaller)`
    //! hands back a handle typed `(specialCaller, ...)`, and dispatches on
    //! `specialCaller`'s direct superclass when `refc` is further up
    //! (JVMS §6.5 `invokespecial`, `Lookup.getDirectMethodCommon`).
    use super::{
        build_method_type_from_descriptor, descriptor_from_method_type, lookup_find_special,
        mh_read_class, special_dispatch_class,
    };
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::{MethodMetadata, NativeClassAccess, NativeHeapAccess};
    use cratonvm_types::{ClassId, ObjectRef, Value};

    fn class(ctx: &mut MockNativeContext, name: &str) -> ClassId {
        ctx.ensure_class_initialized(name)
            .unwrap_or(ClassId::new(0))
    }

    /// `p/Greeter` (interface, default `m(I)I`) <- `p/Grand` <- `p/Mid` <-
    /// `p/Sub`, plus an unrelated `p/Other`.
    fn hierarchy(ctx: &mut MockNativeContext) -> [ClassId; 6] {
        let greeter = class(ctx, "p/Greeter");
        let grand = class(ctx, "p/Grand");
        let mid = class(ctx, "p/Mid");
        let sub = class(ctx, "p/Sub");
        let other = class(ctx, "p/Other");
        let sub_iface = class(ctx, "p/SubIface");
        ctx.set_is_interface(greeter, true);
        ctx.set_is_interface(sub_iface, true);
        ctx.set_declared_methods(
            greeter,
            vec![MethodMetadata {
                name: "m".to_string(),
                descriptor: "(I)I".to_string(),
                access_flags: 0x0001,
                declaring_class_id: greeter,
                exceptions: Vec::new(),
                signature: None,
            }],
        );
        ctx.set_interfaces(grand, vec![greeter]);
        ctx.set_superclass(mid, grand);
        ctx.set_superclass(sub, mid);
        [greeter, grand, mid, sub, other, sub_iface]
    }

    fn find_special(ctx: &mut MockNativeContext, refc: ClassId, caller: ClassId) -> ObjectRef {
        let refc = Value::Object(Some(ctx.get_class_mirror(refc)));
        let caller = Value::Object(Some(ctx.get_class_mirror(caller)));
        let name = Value::Object(Some(ctx.create_string("m")));
        let mt = match build_method_type_from_descriptor(ctx, "(I)I") {
            Ok(Some(mt)) => mt,
            other => panic!("MethodType (I)I: {other:?}"),
        };
        // A null Lookup receiver skips the access gate.
        let args = [
            Value::Object(None),
            refc,
            name,
            Value::Object(Some(mt)),
            caller,
        ];
        match lookup_find_special(ctx, &args) {
            Ok(Some(Value::Object(Some(mh)))) => mh,
            other => panic!("findSpecial: {other:?}"),
        }
    }

    fn type_of(ctx: &MockNativeContext, mh: ObjectRef) -> String {
        match ctx.get_field_by_name(mh, "type") {
            Value::Object(Some(mt)) => descriptor_from_method_type(ctx, mt),
            other => panic!("type field: {other:?}"),
        }
    }

    /// `R12HfixLookupInherited`: `findSpecial(Animal, greet, (int)int, Sub)`
    /// was typed `(Animal,int)int`, so `invokeExact(sub, x)` threw
    /// `WrongMethodTypeException`.
    #[test]
    fn the_receiver_type_is_the_special_caller() {
        let mut ctx = MockNativeContext::new();
        let [_, _, mid, sub, _, _] = hierarchy(&mut ctx);
        let mh = find_special(&mut ctx, mid, sub);
        assert_eq!(type_of(&ctx, mh), "(Lp/Sub;I)I");
        // `Mid` is `Sub`'s direct superclass: dispatch stays on it.
        assert_eq!(mh_read_class(&ctx, mh).as_deref(), Some("p/Mid"));
    }

    /// `findSpecial(Grand, m, Sub)` runs what `super.m()` in `Sub` runs: the
    /// search restarts at `Sub`'s direct superclass `Mid`.
    #[test]
    fn a_refc_above_the_direct_superclass_dispatches_on_the_direct_superclass() {
        let mut ctx = MockNativeContext::new();
        let [_, grand, _, sub, _, _] = hierarchy(&mut ctx);
        let mh = find_special(&mut ctx, grand, sub);
        assert_eq!(type_of(&ctx, mh), "(Lp/Sub;I)I");
        assert_eq!(mh_read_class(&ctx, mh).as_deref(), Some("p/Mid"));
    }

    /// `CRATONVM_MH_FIND_SPECIAL_JDK=0` restores the pre-wave-4 handle:
    /// typed `(refc, ...)`, dispatching on `refc`.
    #[test]
    fn the_kill_switch_restores_the_old_handle() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_FIND_SPECIAL_JDK", Some("0"))],
            || {
                let mut ctx = MockNativeContext::new();
                let [_, grand, _, sub, _, _] = hierarchy(&mut ctx);
                let mh = find_special(&mut ctx, grand, sub);
                assert_eq!(type_of(&ctx, mh), "(Lp/Grand;I)I");
                assert_eq!(mh_read_class(&ctx, mh).as_deref(), Some("p/Grand"));
            },
        );
    }

    #[test]
    fn the_rebind_leaves_every_other_shape_alone() {
        let mut ctx = MockNativeContext::new();
        let [greeter, grand, mid, sub, other, sub_iface] = hierarchy(&mut ctx);
        let d = |ctx: &MockNativeContext, refc: &str, r: ClassId, c: Option<ClassId>| {
            special_dispatch_class(ctx, refc, Some(r), c, "m", "(I)I")
        };
        // The caller itself (a private method), and no caller at all.
        assert_eq!(d(&ctx, "p/Sub", sub, Some(sub)), "p/Sub");
        assert_eq!(d(&ctx, "p/Grand", grand, None), "p/Grand");
        // An interface refc (`I.super.m()`), and an interface caller.
        assert_eq!(d(&ctx, "p/Greeter", greeter, Some(sub)), "p/Greeter");
        assert_eq!(d(&ctx, "p/Grand", grand, Some(sub_iface)), "p/Grand");
        // An unrelated caller: nothing to restart from.
        assert_eq!(d(&ctx, "p/Grand", grand, Some(other)), "p/Grand");
        // Already the direct superclass.
        assert_eq!(d(&ctx, "p/Grand", grand, Some(mid)), "p/Grand");
        assert_eq!(d(&ctx, "p/Grand", grand, Some(sub)), "p/Mid");
        // A method `refc` declares private is not inherited: no restart.
        ctx.set_declared_methods(
            grand,
            vec![MethodMetadata {
                name: "m".to_string(),
                descriptor: "(I)I".to_string(),
                access_flags: 0x0002,
                declaring_class_id: grand,
                exceptions: Vec::new(),
                signature: None,
            }],
        );
        assert_eq!(d(&ctx, "p/Grand", grand, Some(sub)), "p/Grand");
    }
}

#[cfg(test)]
mod r12w5_invoke_declared_cast_tests {
    //! Round 12 wave 5 (hunter3): `MethodHandle.invoke` casts every reference
    //! argument to the handle's DECLARED `type`, receiver included. A
    //! `findSpecial(Mid, m, (int)int, Sub)` handle is typed `(Sub,int)int`, so
    //! `invoke((Object) new Mid(), 3)` is a `ClassCastException` on HotSpot;
    //! the leaf-only cast judged the receiver against `MH_CLASS` (`Mid`) and
    //! let it through (`R12Hunt2FindSpecial`, the `cce` row).
    use super::{
        alloc_method_handle, build_method_type_from_descriptor, declared_cast_params,
        declared_position_judged, invoke_exact_inner_cast_refusal, invoke_reference_cast_refusal,
        lookup_find_special, return_cast_refusal, MH_KIND_SPECIAL, MH_KIND_STATIC, MH_KIND_VIRTUAL,
    };
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, MethodMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
    use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

    fn class(ctx: &mut MockNativeContext, name: &str) -> ClassId {
        ctx.ensure_class_initialized(name)
            .unwrap_or(ClassId::new(0))
    }

    /// `p/Greeter` (interface declaring `m(I)I`, which is what lets the mock's
    /// method resolution find `m` at all) <- `p/Grand` <- `p/Mid` <- `p/Sub`;
    /// returns `(mid, sub)`. The wave-4 tests' hierarchy, minus the extras.
    fn hierarchy(ctx: &mut MockNativeContext) -> (ClassId, ClassId) {
        let greeter = class(ctx, "p/Greeter");
        let grand = class(ctx, "p/Grand");
        let mid = class(ctx, "p/Mid");
        let sub = class(ctx, "p/Sub");
        ctx.set_is_interface(greeter, true);
        ctx.set_declared_methods(
            greeter,
            vec![MethodMetadata {
                name: "m".to_string(),
                descriptor: "(I)I".to_string(),
                access_flags: 0x0001,
                declaring_class_id: greeter,
                exceptions: Vec::new(),
                signature: None,
            }],
        );
        ctx.set_interfaces(grand, vec![greeter]);
        ctx.set_superclass(mid, grand);
        ctx.set_superclass(sub, mid);
        (mid, sub)
    }

    /// `findSpecial(refc, "m", (int)int, caller)` through a null `Lookup`
    /// (which skips the access gate), exactly as the wave-4 tests mint it.
    fn find_special(ctx: &mut MockNativeContext, refc: ClassId, caller: ClassId) -> ObjectRef {
        let refc = Value::Object(Some(ctx.get_class_mirror(refc)));
        let caller = Value::Object(Some(ctx.get_class_mirror(caller)));
        let name = Value::Object(Some(ctx.create_string("m")));
        let mt = match build_method_type_from_descriptor(ctx, "(I)I") {
            Ok(Some(mt)) => mt,
            other => panic!("MethodType (I)I: {other:?}"),
        };
        let args = [
            Value::Object(None),
            refc,
            name,
            Value::Object(Some(mt)),
            caller,
        ];
        match lookup_find_special(ctx, &args) {
            Ok(Some(Value::Object(Some(mh)))) => mh,
            other => panic!("findSpecial: {other:?}"),
        }
    }

    /// The mock resolves no `MethodType` field by name; declare the two
    /// `methodtype_to_descriptor` reads, at the slots
    /// `build_method_type_from_descriptor` writes (`rtype` 0, `ptypes` 1).
    fn make_declared_type_readable(ctx: &mut MockNativeContext, mh: ObjectRef) {
        let mt = match ctx.get_field_by_name(mh, "type") {
            Value::Object(Some(mt)) => mt,
            other => panic!("type field: {other:?}"),
        };
        let cid = ctx.class_id_of_object(mt);
        let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            cid,
            vec![
                field("rtype", "Ljava/lang/Class;", 0),
                field("ptypes", "[Ljava/lang/Class;", 1),
            ],
        );
    }

    /// The class a `ClassCastException` refusal names as its target.
    fn cce_target(refusal: Option<MethodCallFailed>) -> Option<String> {
        match refusal {
            Some(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { message, .. },
            ))) => message.rsplit(" to ").next().map(str::to_string),
            _ => None,
        }
    }

    /// The mock can only give a POSITIVE mismatch for an array (an array is an
    /// `Object`, `Cloneable` and `Serializable` and nothing else), so the
    /// receiver here is an array: what the test pins is WHICH type the refusal
    /// names -- the declared `Sub`, not the leaf's `Mid`.
    #[test]
    fn invoke_casts_the_receiver_to_the_declared_special_caller() {
        let mut ctx = MockNativeContext::new();
        let (mid, sub) = hierarchy(&mut ctx);
        let mh = find_special(&mut ctx, mid, sub);
        make_declared_type_readable(&mut ctx, mh);
        let arr = ctx.new_array(ArrayElementType::Reference, 1);
        let args = [Value::Object(Some(arr)), Value::Int(3)];
        let refusal =
            invoke_reference_cast_refusal(&mut ctx, mh, MH_KIND_SPECIAL, true, false, &args, None);
        assert_eq!(cce_target(refusal).as_deref(), Some("p.Sub"));

        // A `Sub` receiver passes both the declared and the leaf cast.
        let ok = ctx.alloc_object(sub, 1);
        let args = [Value::Object(Some(ok)), Value::Int(3)];
        let refusal =
            invoke_reference_cast_refusal(&mut ctx, mh, MH_KIND_SPECIAL, true, false, &args, None);
        assert!(refusal.is_none());
    }

    /// With the declared pass off, the LEAF pass still refuses the `Mid`: the
    /// handle recorded its `specialCaller` (`MH_SPECIAL_CALLER`).
    #[test]
    fn the_leaf_receiver_of_a_special_handle_is_the_special_caller() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_INVOKE_DECLARED_CAST", Some("0"))],
            || {
                let mut ctx = MockNativeContext::new();
                let (mid, sub) = hierarchy(&mut ctx);
                let mh = find_special(&mut ctx, mid, sub);
                make_declared_type_readable(&mut ctx, mh);
                let arr = ctx.new_array(ArrayElementType::Reference, 1);
                let args = [Value::Object(Some(arr)), Value::Int(3)];
                let refusal = invoke_reference_cast_refusal(
                    &mut ctx,
                    mh,
                    MH_KIND_SPECIAL,
                    true,
                    false,
                    &args,
                    None,
                );
                assert_eq!(cce_target(refusal).as_deref(), Some("p.Sub"));
            },
        );
    }

    /// `findSpecial(Mid, m, (int)int, Sub).asType((Object,int)int)` through
    /// `invokeExact`: the copy's `type` names `Object`, the leaf still `Sub`.
    #[test]
    fn invoke_exact_casts_an_astype_copy_of_a_special_handle_to_the_special_caller() {
        let mut ctx = MockNativeContext::new();
        let (mid, sub) = hierarchy(&mut ctx);
        let mh = find_special(&mut ctx, mid, sub);
        let arr = ctx.new_array(ArrayElementType::Reference, 1);
        let args = [Value::Object(Some(arr)), Value::Int(3)];
        let widened = Some("(Ljava/lang/Object;I)I");
        let refusal =
            invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_SPECIAL, widened, &args);
        assert_eq!(cce_target(refusal).as_deref(), Some("p.Sub"));
        // The handle itself, at its own exact type: not judged.
        let exact = Some("(Lp/Sub;I)I");
        let refusal = invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_SPECIAL, exact, &args);
        assert!(refusal.is_none());
    }

    /// `CRATONVM_MH_INVOKE_DECLARED_CAST=0` with
    /// `CRATONVM_MH_SPECIAL_CALLER_CAST=0` restores the wave-4 cast, which
    /// judges the receiver against `MH_CLASS`.
    #[test]
    fn the_kill_switches_restore_the_leaf_only_cast() {
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_MH_INVOKE_DECLARED_CAST", Some("0")),
                ("CRATONVM_MH_SPECIAL_CALLER_CAST", Some("0")),
            ],
            || {
                let mut ctx = MockNativeContext::new();
                let (mid, sub) = hierarchy(&mut ctx);
                let mh = find_special(&mut ctx, mid, sub);
                make_declared_type_readable(&mut ctx, mh);
                let arr = ctx.new_array(ArrayElementType::Reference, 1);
                let args = [Value::Object(Some(arr)), Value::Int(3)];
                let refusal = invoke_reference_cast_refusal(
                    &mut ctx,
                    mh,
                    MH_KIND_SPECIAL,
                    true,
                    false,
                    &args,
                    None,
                );
                assert_eq!(cce_target(refusal).as_deref(), Some("p.Mid"));
            },
        );
    }

    /// `findVirtual(int[].class, "clone", ..)` is typed `([I)Object` on
    /// HotSpot. The leaf check wrapped the array class name as `L[I;`, read it
    /// back as a class named `[I`, and refused every array receiver.
    #[test]
    fn an_array_class_receiver_is_not_refused() {
        let mut ctx = MockNativeContext::new();
        let mh = handle(&mut ctx, "[I", "()Ljava/lang/Object;", MH_KIND_VIRTUAL);
        let arr = ctx.new_array(ArrayElementType::Int, 2);
        let args = [Value::Object(Some(arr))];
        let refusal =
            invoke_reference_cast_refusal(&mut ctx, mh, MH_KIND_VIRTUAL, true, false, &args, None);
        assert!(refusal.is_none(), "{:?}", cce_target(refusal));
    }

    fn handle(ctx: &mut MockNativeContext, class: &str, desc: &str, kind: i32) -> ObjectRef {
        match alloc_method_handle(ctx, class, "m", desc, kind) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc {class}: {e:?}"),
        }
    }

    /// `findStatic(C, takesSub, (Sub)int).asType((Object)int).invokeExact(x)`:
    /// the adapter's cast from `Object` to `Sub` is owed by `invokeExact`.
    #[test]
    fn invoke_exact_casts_an_astype_copy_to_the_leaf_parameter() {
        let mut ctx = MockNativeContext::new();
        let _ = hierarchy(&mut ctx);
        let mh = handle(&mut ctx, "p/C", "(Lp/Sub;)I", MH_KIND_STATIC);
        let arr = ctx.new_array(ArrayElementType::Reference, 1);
        let args = [Value::Object(Some(arr))];
        let widened = Some("(Ljava/lang/Object;)I");
        let refusal = invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_STATIC, widened, &args);
        assert_eq!(cce_target(refusal).as_deref(), Some("p.Sub"));
        // An unadapted handle is never judged: `invokeExact`'s own call site
        // already typed the argument.
        let exact = Some("(Lp/Sub;)I");
        let refusal = invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_STATIC, exact, &args);
        assert!(refusal.is_none());
        // Unknown declared type, or another arity: not judged.
        assert!(
            invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_STATIC, None, &args).is_none()
        );
        let two = Some("(Ljava/lang/Object;Ljava/lang/Object;)I");
        assert!(
            invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_STATIC, two, &args).is_none()
        );
    }

    /// A virtual handle's leaf receiver is `MH_CLASS`; an array class names
    /// its own descriptor.
    #[test]
    fn invoke_exact_casts_a_widened_virtual_receiver() {
        let mut ctx = MockNativeContext::new();
        let _ = hierarchy(&mut ctx);
        let mh = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
        let arr = ctx.new_array(ArrayElementType::Reference, 1);
        let args = [Value::Object(Some(arr)), Value::Int(3)];
        let widened = Some("(Ljava/lang/Object;I)I");
        let refusal =
            invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_VIRTUAL, widened, &args);
        assert_eq!(cce_target(refusal).as_deref(), Some("p.Mid"));

        let clone = handle(&mut ctx, "[I", "()Ljava/lang/Object;", MH_KIND_VIRTUAL);
        let ints = ctx.new_array(ArrayElementType::Int, 1);
        let args = [Value::Object(Some(ints))];
        let widened = Some("(Ljava/lang/Object;)Ljava/lang/Object;");
        let refusal =
            invoke_exact_inner_cast_refusal(&mut ctx, clone, MH_KIND_VIRTUAL, widened, &args);
        assert!(refusal.is_none());
    }

    /// `CRATONVM_MH_INVOKEEXACT_INNER_CAST=0` restores the uncast dispatch.
    #[test]
    fn the_inner_cast_kill_switch_restores_the_uncast_dispatch() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_INVOKEEXACT_INNER_CAST", Some("0"))],
            || {
                let mut ctx = MockNativeContext::new();
                let _ = hierarchy(&mut ctx);
                let mh = handle(&mut ctx, "p/C", "(Lp/Sub;)I", MH_KIND_STATIC);
                let arr = ctx.new_array(ArrayElementType::Reference, 1);
                let args = [Value::Object(Some(arr))];
                let widened = Some("(Ljava/lang/Object;)I");
                let r =
                    invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_STATIC, widened, &args);
                assert!(r.is_none());
            },
        );
    }

    /// `(String) boxer.invoke(x)` / `boxer.asType((int)String).invokeExact(x)`
    /// on a `(int)Object` handle: the reference RETURN is cast where the JDK's
    /// adapter casts it. An array value is the mock's positive mismatch.
    #[test]
    fn a_reference_return_is_cast_to_the_declared_and_the_call_site_type() {
        let mut ctx = MockNativeContext::new();
        let _ = hierarchy(&mut ctx);
        let greeter = class(&mut ctx, "p/Greeter");
        assert!(ctx.is_interface_class(greeter));
        let arr = ctx.new_array(ArrayElementType::Reference, 1);
        let result: MethodCallResult = Ok(Some(Value::Object(Some(arr))));
        let leaf = "(I)Ljava/lang/Object;";
        let k = MH_KIND_STATIC;
        // INNER: an `asType` copy narrowed the return.
        let sub = Some("(I)Lp/Sub;");
        let r = return_cast_refusal(&mut ctx, &result, k, leaf, sub, None, false);
        assert_eq!(cce_target(r).as_deref(), Some("p.Sub"));
        // OUTER: `invoke`'s call site narrows it.
        let mid_site = Some("(I)Lp/Mid;");
        let r = return_cast_refusal(&mut ctx, &result, k, leaf, Some(leaf), mid_site, false);
        assert_eq!(cce_target(r).as_deref(), Some("p.Mid"));
        // Nothing narrowed, a primitive leaf: not judged.
        let r = return_cast_refusal(&mut ctx, &result, k, leaf, Some(leaf), Some(leaf), false);
        assert!(r.is_none());
        let r = return_cast_refusal(&mut ctx, &result, k, "(I)I", sub, None, false);
        assert!(r.is_none());
        // An interface target IS judged (wave 28: `asType` casts to one;
        // HotSpot 25.0.3 `(Runnable) h.invoke()` on a `String` result is a
        // `ClassCastException`), except the inner cast of an
        // `explicitCastArguments` stamp, which does not cast to one.
        let greeter = Some("(I)Lp/Greeter;");
        let r = return_cast_refusal(&mut ctx, &result, k, leaf, Some(leaf), greeter, false);
        assert_eq!(cce_target(r).as_deref(), Some("p.Greeter"));
        let r = return_cast_refusal(&mut ctx, &result, k, leaf, greeter, None, false);
        assert_eq!(cce_target(r).as_deref(), Some("p.Greeter"));
        let r = return_cast_refusal(&mut ctx, &result, k, leaf, greeter, None, true);
        assert!(r.is_none());
        let r = return_cast_refusal(&mut ctx, &result, k, leaf, Some(leaf), greeter, true);
        assert_eq!(cce_target(r).as_deref(), Some("p.Greeter"));
        // The switch.
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_RETURN_CAST", Some("0"))],
            || {
                let r = return_cast_refusal(&mut ctx, &result, k, leaf, sub, None, false);
                assert!(r.is_none());
            },
        );
    }

    #[test]
    fn only_an_aligned_declared_type_is_judged() {
        assert_eq!(
            declared_cast_params("(Lp/Sub;I)I", 2),
            Some(vec!["Lp/Sub;".to_string(), "I".to_string()])
        );
        // A collector (or a spread) presents another arity: not judged.
        assert_eq!(declared_cast_params("(Lp/Sub;I)I", 3), None);
        // The fabricated `()V` stand-in, and nothing to cast.
        assert_eq!(declared_cast_params("()V", 1), None);
        assert_eq!(declared_cast_params("()V", 0), None);
        assert_eq!(declared_cast_params("not a descriptor", 1), None);
    }

    #[test]
    fn the_leaf_pass_skips_only_an_identical_target() {
        let declared = vec!["Lp/Sub;".to_string(), "I".to_string()];
        let d = Some(declared.as_slice());
        assert!(declared_position_judged(d, 0, "Lp/Sub;"));
        assert!(!declared_position_judged(d, 0, "Lp/Mid;"));
        assert!(declared_position_judged(d, 1, "I"));
        assert!(!declared_position_judged(d, 2, "I"));
        assert!(!declared_position_judged(None, 0, "Lp/Sub;"));
    }
}

/// Interpreter round i1 wave 26, lane L4: the leaf arms' fallible argument
/// conversion (`adapt_leaf_args`) and the placeholders the adapters pass
/// (`mh_zero_value`, `loop_variable_shape`). The end-to-end rows are
/// `tools/probes/interp/L4/L4W26AdapterLeafConversions.java`.
#[cfg(test)]
mod w26_leaf_conversion_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    fn wrapper(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
        let o = match ctx.new_object(class).unwrap() {
            Some(Value::Object(Some(o))) => o,
            _ => unreachable!(),
        };
        ctx.set_field(o, 0, payload);
        Value::Object(Some(o))
    }

    fn is_cce(r: &Result<Vec<Value>, MethodCallFailed>) -> bool {
        matches!(
            r,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { .. }
            )))
        )
    }

    fn is_npe(r: &Result<Vec<Value>, MethodCallFailed>) -> bool {
        matches!(
            r,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { .. }
            )))
        )
    }

    /// The page's rows at the leaf: a `String`, a non-widening `Long` and a
    /// `null` at an `int` parameter are refused; a `Short` converts. Every
    /// pin taken is released on both outcomes.
    #[test]
    fn a_reference_at_a_primitive_parameter_converts_or_is_refused() {
        let mut ctx = MockNativeContext::new();
        let s = wrapper(&mut ctx, "java/lang/String", Value::Int(0));
        let long = wrapper(&mut ctx, "java/lang/Long", Value::Long(2));
        let short = wrapper(&mut ctx, "java/lang/Short", Value::Int(2));
        let one = Value::Int(1);
        assert!(is_cce(&adapt_leaf_args(
            &mut ctx,
            &[one, s],
            "(II)I",
            None,
            false
        )));
        assert!(is_cce(&adapt_leaf_args(
            &mut ctx,
            &[one, long],
            "(II)I",
            None,
            false
        )));
        assert!(is_npe(&adapt_leaf_args(
            &mut ctx,
            &[one, Value::Object(None)],
            "(II)I",
            None,
            false
        )));
        assert_eq!(
            adapt_leaf_args(&mut ctx, &[one, short], "(II)I", None, false).unwrap(),
            vec![one, Value::Int(2)]
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    /// What `adapt_single_arg` did and still does: a widening wrapper is
    /// unboxed AND widened, a raw primitive is widened, a reference parameter
    /// keeps whatever it is given (a `null` included).
    #[test]
    fn the_accepted_shapes_are_unchanged() {
        let mut ctx = MockNativeContext::new();
        let int = wrapper(&mut ctx, "java/lang/Integer", Value::Int(7));
        let chr = wrapper(&mut ctx, "java/lang/Character", Value::Int(0x41));
        let s = wrapper(&mut ctx, "java/lang/String", Value::Int(0));
        let out = adapt_leaf_args(
            &mut ctx,
            &[int, Value::Int(3), chr, s, Value::Object(None)],
            "(JDILjava/lang/Object;Ljava/lang/String;)V",
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            out,
            vec![
                Value::Long(7),
                Value::Double(3.0),
                Value::Int(0x41),
                s,
                Value::Object(None)
            ]
        );
    }

    /// A site that typed the argument as a primitive passed it raw; a
    /// reference there is the VM's own and keeps the plain unbox.
    #[test]
    fn a_primitive_site_keeps_the_plain_unbox() {
        let mut ctx = MockNativeContext::new();
        let long = wrapper(&mut ctx, "java/lang/Long", Value::Long(2));
        let site = vec!["I".to_string()];
        assert_eq!(
            adapt_leaf_args(&mut ctx, &[long], "(I)V", Some(&site), false).unwrap(),
            vec![Value::Long(2)]
        );
        assert!(is_cce(&adapt_leaf_args(
            &mut ctx,
            &[long],
            "(I)V",
            None,
            false
        )));
    }

    /// Under an `explicitCastArguments` stamp nothing is refused: a `null` is
    /// the zero, a wrapper is unboxed and cast (narrowing included), a raw
    /// value is cast, `boolean` takes the low-order bit.
    #[test]
    fn an_explicit_cast_scope_casts_instead_of_refusing() {
        let mut ctx = MockNativeContext::new();
        let long = wrapper(&mut ctx, "java/lang/Long", Value::Long(0x1_0000_0005));
        let out = adapt_leaf_args(
            &mut ctx,
            &[
                Value::Object(None),
                long,
                Value::Int(300),
                Value::Double(2.9),
                Value::Int(6),
            ],
            "(IIBIZ)V",
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            out,
            vec![
                Value::Int(0),
                Value::Int(5),
                Value::Int(44),
                Value::Int(2),
                Value::Int(0)
            ]
        );
        assert_eq!(explicit_cast_primitive(Value::Int(7), "J"), Value::Long(7));
        assert_eq!(
            explicit_cast_primitive(Value::Int(-1), "C"),
            Value::Int(0xFFFF)
        );
    }

    /// The marking bits share `MH_VARARGS`: setting one keeps the other.
    #[test]
    fn the_explicit_cast_mark_and_the_varargs_mark_are_independent() {
        let mut ctx = MockNativeContext::new();
        let h = alloc_method_handle(&mut ctx, "C", "m", "(I)I", MH_KIND_STATIC).unwrap();
        assert!(!mh_is_explicit_cast(&ctx, h));
        mh_set_varargs_collector(&ctx, h, true);
        mh_mark_explicit_cast(&ctx, h);
        assert!(mh_is_varargs_collector(&ctx, h));
        assert!(mh_is_explicit_cast(&ctx, h));
        mh_set_varargs_collector(&ctx, h, false);
        assert!(!mh_is_varargs_collector(&ctx, h));
        assert!(
            mh_is_explicit_cast(&ctx, h),
            "clearing varargs keeps the cast mark"
        );
    }

    /// `MH_MARKS` is the one table of the marking bits: distinct powers of
    /// two, and every `MH_MARK_*` constant of the file is a row (wave 51,
    /// lane L4; the exact-invoker / cast-chain collision of wave 50).
    #[test]
    fn the_marking_bits_are_one_table() {
        let mut seen = 0i32;
        for (name, bit, _) in MH_MARKS {
            assert!(
                bit > 0 && bit.count_ones() == 1,
                "{name}: {bit} is not one bit"
            );
            assert_eq!(seen & bit, 0, "{name}: bit {bit} collides");
            seen |= bit;
        }
        assert_eq!(MH_MARKS_DIVERT_DIRECT_LANE & !seen, 0);
        assert_eq!(MH_MARKS_INHERITED & !seen, 0);
        // Every `MH_MARK_*` constant declared in this file is in the table.
        let src = LANG_INVOKE_SRC;
        let decl = concat!("const ", "MH_MARK_");
        let declared = src.matches(decl).count();
        assert_eq!(
            declared,
            MH_MARKS.len(),
            "a MH_MARK_* constant without an MH_MARKS row"
        );
    }

    /// No `MH_VARARGS` store outside `mh_write_marking_bits` (a source scan,
    /// whitespace-insensitive, as the other ratchets do).
    #[test]
    fn the_marking_bits_have_one_writer() {
        let src: String = LANG_INVOKE_SRC
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let store = concat!("MH_VARARGS", ",Value::");
        assert_eq!(
            src.matches(store).count(),
            1,
            "a second writer of MH_VARARGS"
        );
    }

    /// A copy keeps every marking bit `MH_MARKS` says it inherits.
    #[test]
    fn a_copy_keeps_the_inherited_marking_bits() {
        let mut ctx = MockNativeContext::new();
        let h = alloc_method_handle(&mut ctx, "C", "m", "(I)I", MH_KIND_STATIC).unwrap();
        mh_set_varargs_collector(&ctx, h, true);
        mh_mark_explicit_cast(&ctx, h);
        mh_mark_exact_invoker(&ctx, h);
        let before = mh_marking_bits(&ctx, h);
        let copy = mh_clone_handle(&mut ctx, h).expect("a synthetic handle copies");
        assert_eq!(mh_marking_bits(&ctx, copy), before & MH_MARKS_INHERITED);
    }

    /// A bound value leads the list the site does not name.
    #[test]
    fn site_params_align_past_a_bound_value() {
        assert_eq!(
            leaf_site_params(Some("(Ljava/lang/Object;I)V"), 1, 3),
            Some(vec![
                String::new(),
                "Ljava/lang/Object;".to_string(),
                "I".to_string()
            ])
        );
        assert_eq!(
            leaf_site_params(Some("(I)V"), 0, 2),
            None,
            "a reshaped list has no site"
        );
        assert_eq!(leaf_site_params(None, 0, 0), None);
    }

    /// `tryFinally`'s result slot and a loop variable with no `init`.
    #[test]
    fn placeholders_are_the_primitive_zero() {
        assert_eq!(mh_zero_value("I"), Value::Int(0));
        assert_eq!(mh_zero_value("J"), Value::Long(0));
        assert_eq!(mh_zero_value("D"), Value::Double(0.0));
        assert_eq!(mh_zero_value("Ljava/lang/String;"), Value::Object(None));
        assert_eq!(mh_zero_value(""), Value::Object(None));

        let mut ctx = MockNativeContext::new();
        let mut body = |desc: &str| {
            let h = alloc_method_handle(&mut ctx, "C", "b", desc, MH_KIND_STATIC).unwrap();
            // No `type`: the shape is read from `MH_DESC`.
            ctx.set_field_by_name(h, "type", Value::Object(None));
            loop_variable_shape(&mut ctx, Some(h))
        };
        assert_eq!(body("(II)I"), (false, Some(Value::Int(0))));
        assert_eq!(body("(I)V"), (true, None));
        assert_eq!(
            body("(Ljava/lang/String;I)Ljava/lang/String;"),
            (false, None)
        );
    }
}

/// Wave 27, lane L4: `asType`'s reference -> reference cast at a leaf
/// ([`leaf_reference_refusal`], [`leaf_receiver_refusal`]). The mock context
/// has no by-name hierarchy walk (`class_assignable_to_name` answers `None`),
/// so the only refusals it can produce are the ones the SHAPE settles: an
/// array where a class is wanted, a non-array or a primitive array where
/// another array type is wanted. Everything else must fail open. The
/// hierarchy rows are `tools/probes/interp/L4/L4W27AdapterLeafReferenceCasts.java`.
#[cfg(test)]
mod w27_leaf_reference_cast_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    fn object(ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
        match ctx.new_object(class).unwrap() {
            Some(Value::Object(Some(o))) => o,
            _ => unreachable!(),
        }
    }

    fn is_cce(r: &Result<Vec<Value>, MethodCallFailed>) -> bool {
        matches!(
            r,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { .. }
            )))
        )
    }

    #[test]
    fn shapes_the_hierarchy_cannot_contradict_are_refused() {
        let mut ctx = MockNativeContext::new();
        let s = Value::Object(Some(object(&mut ctx, "java/lang/String")));
        let ints = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
        let refs = Value::Object(Some(ctx.new_array(ArrayElementType::Reference, 1)));
        // A non-array at an array parameter.
        assert!(is_cce(&adapt_leaf_args(
            &mut ctx,
            &[s],
            "([Ljava/lang/String;)V",
            None,
            false
        )));
        // A primitive array at a reference-array parameter, and at a class.
        assert!(is_cce(&adapt_leaf_args(
            &mut ctx,
            &[ints],
            "([Ljava/lang/Object;)V",
            None,
            false
        )));
        assert!(is_cce(&adapt_leaf_args(
            &mut ctx,
            &[ints],
            "(Ljava/lang/String;)V",
            None,
            false
        )));
        // A reference array at `int[]`, after a converted primitive.
        assert!(is_cce(&adapt_leaf_args(
            &mut ctx,
            &[Value::Int(1), refs],
            "(I[I)V",
            None,
            false
        )));
        // An explicit cast to a CLASS still casts.
        assert!(is_cce(&adapt_leaf_args(
            &mut ctx,
            &[ints],
            "(Ljava/lang/String;)V",
            None,
            true
        )));
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    #[test]
    fn what_the_cast_cannot_contradict_passes() {
        let mut ctx = MockNativeContext::new();
        let s = Value::Object(Some(object(&mut ctx, "java/lang/String")));
        let ints = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
        let refs = Value::Object(Some(ctx.new_array(ArrayElementType::Reference, 1)));
        let null = Value::Object(None);
        let out = adapt_leaf_args(
            &mut ctx,
            &[s, null, s, ints, refs, ints, refs],
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/Object;[I[Ljava/lang/String;Ljava/lang/Cloneable;[Ljava/lang/Object;)V",
            None,
            false,
        )
        .unwrap();
        assert_eq!(out, vec![s, null, s, ints, refs, ints, refs]);
        // A list that does not line up 1:1 is not judged (the varargs repair's
        // flat lists): the `String` at the array position passes.
        assert_eq!(
            adapt_leaf_args(&mut ctx, &[s, s], "([Ljava/lang/String;)V", None, false).unwrap(),
            vec![s, s]
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    #[test]
    fn a_receiver_is_judged_against_its_class() {
        let mut ctx = MockNativeContext::new();
        let s = object(&mut ctx, "java/lang/String");
        let ints = ctx.new_array(ArrayElementType::Int, 1);
        assert!(leaf_receiver_refusal(&mut ctx, s, "java/lang/String", false).is_none());
        assert!(leaf_receiver_refusal(&mut ctx, ints, "java/lang/String", false).is_some());
        assert!(leaf_receiver_refusal(&mut ctx, ints, "[I", false).is_none());
        assert!(leaf_receiver_refusal(&mut ctx, s, "[I", false).is_some());
        // No class to judge against, or `Object`: never refused.
        assert!(leaf_receiver_refusal(&mut ctx, ints, "", false).is_none());
        assert!(leaf_receiver_refusal(&mut ctx, ints, "java/lang/Object", false).is_none());
    }

    /// Interpreter round i2 wave 62, lane L4 (`CRATONVM_MH_INSTANCE_OWNER_ID`):
    /// the owner's id admits its own class; a nameless owner (a lambda
    /// proxy's) refuses another class itself; a named owner leaves a mismatch
    /// to the by-name cast, so the switch moves no named-class answer.
    #[test]
    fn an_owner_id_judges_the_receiver() {
        let mut ctx = MockNativeContext::new();
        let s = object(&mut ctx, "java/lang/String");
        let string = ctx.class_id_of_object(s);
        let ints = ctx.new_array(ArrayElementType::Int, 1);
        let i = object(&mut ctx, "java/lang/Integer");
        assert!(matches!(
            owner_receiver_verdict(&mut ctx, s, string, "java/lang/String"),
            Some(None)
        ));
        assert!(owner_receiver_verdict(&mut ctx, i, string, "java/lang/String").is_none());
        assert!(owner_receiver_verdict(&mut ctx, ints, string, "java/lang/String").is_none());
        assert!(matches!(
            owner_receiver_verdict(&mut ctx, i, string, ""),
            Some(Some(_))
        ));
        assert!(matches!(
            owner_receiver_verdict(&mut ctx, s, string, ""),
            Some(None)
        ));
    }

    /// An invoker's own type (HotSpot 25.0.3, `L4W27InvokerTypes.java`): the
    /// target handle leads, and a spread invoker's trailing parameters are
    /// one `Object[]`.
    #[test]
    fn an_invokers_type_leads_with_the_target_handle() {
        let mh = "Ljava/lang/invoke/MethodHandle;";
        assert_eq!(
            invoker_type_descriptor("([Ljava/lang/Object;)Ljava/lang/Object;", None),
            Some(format!("({mh}[Ljava/lang/Object;)Ljava/lang/Object;"))
        );
        assert_eq!(
            invoker_type_descriptor("(Ljava/lang/String;I)Ljava/lang/String;", Some(0)),
            Some(format!("({mh}[Ljava/lang/Object;)Ljava/lang/String;"))
        );
        assert_eq!(
            invoker_type_descriptor(
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                Some(1)
            ),
            Some(format!(
                "({mh}Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;"
            ))
        );
        assert_eq!(
            invoker_type_descriptor("(II)I", Some(2)),
            Some(format!("({mh}II[Ljava/lang/Object;)I"))
        );
        assert_eq!(
            invoker_type_descriptor("()V", None),
            Some(format!("({mh})V"))
        );
        assert_eq!(
            invoker_type_descriptor("(I)V", Some(2)),
            None,
            "lead past the arity"
        );
        assert_eq!(invoker_type_descriptor("(I)V", Some(-1)), None);
        assert_eq!(invoker_type_descriptor("", None), None);
    }
}

/// Interpreter round i2 wave 63, lane L4: stage 2 of the owner-by-id proposal
/// (`CRATONVM_MH_OWNER_ID_AUTHORITY`). The helpers are flag-free or take the
/// switch as a parameter, so both answers are asked here; the end-to-end rows
/// are `tools/probes/interp/L4/L4W63ForeignOwnerCast.java`.
#[cfg(test)]
mod i63_l4_owner_id_authority_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// `p/Base` of loader 3, a second `p/Base` of loader 4 and its subclass
    /// `p/Sub` (loader 4), a third `p/Base` whose loader is unknown.
    struct Namesakes {
        base_a: ClassId,
        base_b: ClassId,
        sub_b: ClassId,
        base_unknown: ClassId,
    }

    fn namesakes(ctx: &mut MockNativeContext) -> Namesakes {
        let base_a = ctx.ensure_class_initialized("p/Base").expect("p/Base");
        let base_b = ctx.declare_second_copy("p/Base");
        let sub_b = ctx.declare_second_copy("p/Sub");
        let base_unknown = ctx.declare_second_copy("p/Base");
        ctx.set_superclass(sub_b, base_b);
        ctx.set_loader_id_override(base_a, 3);
        ctx.set_loader_id_override(base_b, 4);
        ctx.set_loader_id_override(sub_b, 4);
        Namesakes {
            base_a,
            base_b,
            sub_b,
            base_unknown,
        }
    }

    fn cce_message(r: Option<Option<MethodCallFailed>>) -> Option<String> {
        match r {
            Some(Some(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { message },
            )))) => Some(message),
            _ => None,
        }
    }

    #[test]
    fn another_loaders_namesake_is_foreign() {
        let mut ctx = MockNativeContext::new();
        let n = namesakes(&mut ctx);
        assert!(owner_foreign_namesake(&ctx, n.base_b, n.base_a, "p/Base"));
        assert!(
            owner_foreign_namesake(&ctx, n.sub_b, n.base_a, "p/Base"),
            "a namesake's subclass"
        );
        assert!(
            !owner_foreign_namesake(&ctx, n.base_a, n.base_a, "p/Base"),
            "the owner itself"
        );
        // A loader the VM does not know (a stand-in) keeps the by-name answer.
        assert!(!owner_foreign_namesake(
            &ctx,
            n.base_unknown,
            n.base_a,
            "p/Base"
        ));
        assert!(!owner_foreign_namesake(
            &ctx,
            n.base_b,
            n.base_unknown,
            "p/Base"
        ));
        // Another name, no name, an array class: never.
        let int = ctx
            .ensure_class_initialized("java/lang/Integer")
            .expect("Integer");
        ctx.set_loader_id_override(int, 0);
        assert!(!owner_foreign_namesake(&ctx, int, n.base_a, "p/Base"));
        assert!(!owner_foreign_namesake(&ctx, n.base_b, n.base_a, ""));
        assert!(!owner_foreign_namesake(
            &ctx,
            n.base_b,
            n.base_a,
            "[Lp/Base;"
        ));
        // The same loader's second copy is not judged foreign.
        ctx.set_loader_id_override(n.base_unknown, 3);
        assert!(!owner_foreign_namesake(
            &ctx,
            n.base_unknown,
            n.base_a,
            "p/Base"
        ));
    }

    #[test]
    fn the_authority_refuses_a_namesake_receiver_with_hotspots_message() {
        let mut ctx = MockNativeContext::new();
        let n = namesakes(&mut ctx);
        let a = ctx.alloc_object(n.base_a, 2);
        let b = ctx.alloc_object(n.base_b, 2);
        let sb = ctx.alloc_object(n.sub_b, 2);
        // The owner's own instance is admitted by id, either way.
        assert!(matches!(
            owner_receiver_verdict_with(&mut ctx, a, n.base_a, "p/Base", true),
            Some(None)
        ));
        assert!(matches!(
            owner_receiver_verdict_with(&mut ctx, a, n.base_a, "p/Base", false),
            Some(None)
        ));
        // Off: a named owner's mismatch is left to the by-name cast (stage 1).
        assert!(owner_receiver_verdict_with(&mut ctx, b, n.base_a, "p/Base", false).is_none());
        assert!(owner_receiver_verdict_with(&mut ctx, sb, n.base_a, "p/Base", false).is_none());
        // On: refused, named as HotSpot's `Class.cast` names them.
        assert_eq!(
            cce_message(owner_receiver_verdict_with(
                &mut ctx, b, n.base_a, "p/Base", true
            ))
            .as_deref(),
            Some("Cannot cast p.Base to p.Base")
        );
        assert_eq!(
            cce_message(owner_receiver_verdict_with(
                &mut ctx, sb, n.base_a, "p/Base", true
            ))
            .as_deref(),
            Some("Cannot cast p.Sub to p.Base")
        );
        // An unrelated class is still the by-name cast's to judge.
        let s = ctx
            .ensure_class_initialized("java/lang/String")
            .expect("String");
        let str_obj = ctx.alloc_object(s, 1);
        assert!(owner_receiver_verdict_with(&mut ctx, str_obj, n.base_a, "p/Base", true).is_none());
    }

    #[test]
    fn a_field_slot_resolves_in_the_owner_by_id() {
        let mut ctx = MockNativeContext::new();
        let field = |cid: ClassId, slot_index: usize| FieldMetadata {
            name: "cellValueZ".to_string(),
            descriptor: "I".to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: cid,
            is_static: false,
        };
        let by_name = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        ctx.set_declared_fields(by_name, vec![field(by_name, 3)]);
        let second = ctx.declare_second_copy("p/Cell");
        ctx.set_declared_fields(second, vec![field(second, 1)]);
        // The owner's own layout, not whichever copy the name finds.
        assert_eq!(
            mh_instance_field_slot(&ctx, Some(second), "p/Cell", "cellValueZ"),
            Some(1)
        );
        assert_eq!(
            mh_instance_field_slot(&ctx, Some(by_name), "p/Cell", "cellValueZ"),
            Some(3)
        );
        // No owner: the by-name lookup, as before stage 2.
        assert_eq!(
            mh_instance_field_slot(&ctx, None, "p/Cell", "cellValueZ"),
            Some(3)
        );
        // An owner that does not declare it falls back to the name.
        let other = ctx.declare_second_copy("p/Other");
        assert_eq!(
            mh_instance_field_slot(&ctx, Some(other), "p/Cell", "cellValueZ"),
            Some(3)
        );
    }
}

/// Interpreter round i2 wave 64, lane L4: an instance field handle carries its
/// slot from mint (`CRATONVM_MH_FIELD_SLOT_FROM_MINT`). The helpers take the
/// switch as a parameter; the end-to-end rows are
/// `tools/probes/interp/L4/L4W64FieldSlotFromMint.java`.
#[cfg(test)]
mod i64_l4_field_slot_from_mint_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn field(cid: ClassId, slot_index: usize) -> FieldMetadata {
        FieldMetadata {
            name: "cellValueZ".to_string(),
            descriptor: "I".to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: cid,
            is_static: false,
        }
    }

    fn getter(ctx: &mut MockNativeContext) -> ObjectRef {
        // The descriptor is irrelevant to the slot helpers (the mock builds no
        // `MethodType` of an unknown class).
        alloc_method_handle(ctx, "p/Cell", "cellValueZ", "()I", MH_KIND_GETTER).unwrap()
    }

    #[test]
    fn the_packed_state_round_trips_and_never_reads_as_linked() {
        for slot in [0usize, 1, 7, 0xFFFF] {
            let state = field_link_minted_state(slot).expect("fits");
            assert_eq!(state & FL_LINKED, 0, "a minted slot is not a link");
            assert_eq!(field_link_state_minted_slot(state), Some(slot));
            assert_eq!((state >> FL_LINKS_SHIFT) & 0x7, 0, "no relink counted");
        }
        assert_eq!(field_link_minted_state(0x1_0000), None);
        assert_eq!(field_link_state_minted_slot(0), None);
        // A link without a minted slot carries none.
        let linked = FL_LINKED | (5 << 16) | 3 | (1 << FL_LINKS_SHIFT);
        assert_eq!(field_link_state_minted_slot(linked), None);
    }

    #[test]
    fn a_link_keeps_the_minted_bit_only_for_the_same_slot() {
        let old = field_link_minted_state(3).unwrap();
        let linked = FL_LINKED | (5 << 16) | 3 | (1 << FL_LINKS_SHIFT);
        let kept = field_link_state_keeping_mint(linked, old, 3);
        assert_eq!(field_link_state_minted_slot(kept), Some(3));
        assert_ne!(kept & FL_LINKED, 0);
        assert_eq!((kept >> 16) & 0xFF, 5, "type index intact");
        assert_eq!((kept >> FL_LINKS_SHIFT) & 0x7, 1, "relink count intact");
        // Another slot, or no minted slot: the link alone.
        assert_eq!(field_link_state_keeping_mint(linked, old, 4), linked);
        assert_eq!(field_link_state_keeping_mint(linked, 0, 3), linked);
    }

    #[test]
    fn a_copy_keeps_the_minted_slot_and_drops_the_link() {
        let minted_linked =
            FL_MINT_SLOT | FL_LINKED | FL_SETTER | (5 << 16) | 3 | (2 << FL_LINKS_SHIFT);
        let kept = field_link_state_kept_by_copy(minted_linked);
        assert_eq!(kept, field_link_minted_state(3).unwrap());
        assert_eq!(kept & FL_LINKED, 0);
        let plain_linked = FL_LINKED | (5 << 16) | 3 | (2 << FL_LINKS_SHIFT);
        assert_eq!(field_link_state_kept_by_copy(plain_linked), 0);
        assert_eq!(field_link_state_kept_by_copy(0), 0);
    }

    #[test]
    fn a_mint_records_the_owners_slot_only_when_on() {
        let mut ctx = MockNativeContext::new();
        let by_name = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        ctx.set_declared_fields(by_name, vec![field(by_name, 3)]);
        let second = ctx.declare_second_copy("p/Cell");
        ctx.set_declared_fields(second, vec![field(second, 1)]);

        // Off: nothing recorded, the link slot stays `0`.
        let mh = getter(&mut ctx);
        mh_record_minted_field_slot_with(&ctx, mh, Some(second), "cellValueZ", false);
        assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(0)));
        assert_eq!(mh_minted_field_slot_with(&ctx, mh, true), None);

        // On: the OWNER's slot (the second copy's), not the name's.
        mh_record_minted_field_slot_with(&ctx, mh, Some(second), "cellValueZ", true);
        assert_eq!(mh_minted_field_slot_with(&ctx, mh, true), Some(1));
        // The reader asks the switch too.
        assert_eq!(mh_minted_field_slot_with(&ctx, mh, false), None);

        // No owner, or an owner that does not declare the name: nothing.
        let mh2 = getter(&mut ctx);
        mh_record_minted_field_slot_with(&ctx, mh2, None, "cellValueZ", true);
        assert_eq!(mh_minted_field_slot_with(&ctx, mh2, true), None);
        let other = ctx.declare_second_copy("p/Other");
        mh_record_minted_field_slot_with(&ctx, mh2, Some(other), "cellValueZ", true);
        assert_eq!(mh_minted_field_slot_with(&ctx, mh2, true), None);
    }

    #[test]
    fn a_narrow_handle_records_and_reads_nothing() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        ctx.set_declared_fields(cell, vec![field(cell, 2)]);
        let narrow = ctx.alloc_object(cell, MH_SLOT_COUNT - 1);
        mh_record_minted_field_slot_with(&ctx, narrow, Some(cell), "cellValueZ", true);
        assert_eq!(mh_minted_field_slot_with(&ctx, narrow, true), None);
    }

    #[test]
    fn a_clone_keeps_the_minted_slot() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        ctx.set_declared_fields(cell, vec![field(cell, 2)]);
        let mh = getter(&mut ctx);
        mh_record_minted_field_slot_with(&ctx, mh, Some(cell), "cellValueZ", true);
        let copy = mh_clone_handle(&mut ctx, mh).expect("copy");
        assert_eq!(mh_minted_field_slot_with(&ctx, copy, true), Some(2));
        // A handle with no minted slot copies to `0`, as before.
        let plain = getter(&mut ctx);
        let copy = mh_clone_handle(&mut ctx, plain).expect("copy");
        assert!(matches!(ctx.get_field(copy, MH_FIELD_LINK), Value::Int(0)));
    }
}

/// Interpreter round i2 wave 65, lane L4: a bound field handle keeps its owner
/// and minted slot (`CRATONVM_MH_BOUND_FIELD_OWNER`), and an owned
/// field-accessor link admits the owner's subclasses
/// (`CRATONVM_MH_FIELD_LINK_OWNER_ADMITS`). The helpers take the switch (and
/// the owner) as parameters; the end-to-end rows are
/// `tools/probes/interp/L4/L4W65BoundFieldOwner.java` and
/// `tools/probes/interp/L4/L4W65FieldLinkSubclasses.java`.
#[cfg(test)]
mod i65_l4_bound_field_and_owner_link_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn field(cid: ClassId, slot_index: usize) -> FieldMetadata {
        FieldMetadata {
            name: "cellValueZ".to_string(),
            descriptor: "I".to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: cid,
            is_static: false,
        }
    }

    /// A handle of `kind` (the descriptor the carry judges is passed to it
    /// separately: the mock builds no `MethodType` of an unknown class).
    fn handle(ctx: &mut MockNativeContext, kind: i32) -> ObjectRef {
        alloc_method_handle(ctx, "p/Cell", "cellValueZ", "()I", kind).unwrap()
    }

    #[test]
    fn the_carry_is_an_unbound_instance_field_handles_owner_and_minted_slot() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        ctx.set_declared_fields(cell, vec![field(cell, 2)]);
        let getter = handle(&mut ctx, MH_KIND_GETTER);
        let g = "(Lp/Cell;)I";

        // Off, or no recorded owner: nothing.
        assert_eq!(
            bound_field_carry_with(&ctx, getter, MH_KIND_GETTER, g, Some(cell), false),
            None
        );
        assert_eq!(
            bound_field_carry_with(&ctx, getter, MH_KIND_GETTER, g, None, true),
            None
        );

        // The owner alone: the narrow bound carrier is enough.
        let carry = bound_field_carry_with(&ctx, getter, MH_KIND_GETTER, g, Some(cell), true)
            .expect("owner carried");
        assert_eq!(
            carry,
            BoundFieldCarry {
                owner: cell,
                link_state: 0
            }
        );
        assert!(!carry.needs_link_slots());

        // A minted slot rides too, and asks for the link slots.
        mh_record_minted_field_slot_with(&ctx, getter, Some(cell), "cellValueZ", true);
        let carry = bound_field_carry_with(&ctx, getter, MH_KIND_GETTER, g, Some(cell), true)
            .expect("owner and slot carried");
        assert_eq!(carry.link_state, field_link_minted_state(2).unwrap());
        assert!(carry.needs_link_slots());

        // A LINKED source hands over the minted slot only, never its link.
        let linked = field_link_minted_state(2).unwrap()
            | FL_LINKED
            | FL_OWNER_ADMITS
            | (3 << 16)
            | (2 << FL_LINKS_SHIFT);
        ctx.set_field(getter, MH_FIELD_LINK, Value::Int(linked));
        let carry = bound_field_carry_with(&ctx, getter, MH_KIND_GETTER, g, Some(cell), true)
            .expect("carried");
        assert_eq!(carry.link_state, field_link_minted_state(2).unwrap());

        // An instance setter carries; the static shapes and the other kinds
        // do not.
        let setter = handle(&mut ctx, MH_KIND_SETTER);
        assert!(bound_field_carry_with(
            &ctx,
            setter,
            MH_KIND_SETTER,
            "(Lp/Cell;I)V",
            Some(cell),
            true
        )
        .is_some());
        assert_eq!(
            bound_field_carry_with(&ctx, setter, MH_KIND_SETTER, "(I)V", Some(cell), true),
            None
        );
        assert_eq!(
            bound_field_carry_with(&ctx, getter, MH_KIND_GETTER, "()I", Some(cell), true),
            None
        );
        // Classless (the mock builds no receiver-typed `MethodType`).
        let virt = alloc_method_handle(&mut ctx, "", "m", "()I", MH_KIND_VIRTUAL).unwrap();
        assert_eq!(
            bound_field_carry_with(&ctx, virt, MH_KIND_VIRTUAL, g, Some(cell), true),
            None
        );

        // An already-bound source carries nothing (its receiver is captured).
        let r = ctx.alloc_object(cell, 4);
        ctx.set_field(setter, MH_BOUND, Value::Object(Some(r)));
        assert_eq!(
            bound_field_carry_with(
                &ctx,
                setter,
                MH_KIND_SETTER,
                "(Lp/Cell;I)V",
                Some(cell),
                true
            ),
            None
        );
    }

    #[test]
    fn the_carry_is_written_as_wide_as_the_copy_allows() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let state = field_link_minted_state(2).unwrap();
        let carry = BoundFieldCarry {
            owner: cell,
            link_state: state,
        };

        // A copy with the link slots: the owner and the minted slot.
        let wide = handle(&mut ctx, MH_KIND_GETTER);
        mh_write_bound_field_carry(&ctx, wide, carry);
        assert_eq!(
            ctx.get_field(wide, MH_STATIC_OWNER),
            Value::Int(cell.as_u32() as i32)
        );
        assert_eq!(mh_minted_field_slot_with(&ctx, wide, true), Some(2));
        assert_eq!(ctx.get_field(wide, MH_FIELD_LINK), Value::Int(state));

        // The narrow bound carrier: the owner alone.
        let narrow = ctx.alloc_object(cell, MH_STATIC_OWNER + 1);
        mh_write_bound_field_carry(&ctx, narrow, carry);
        assert_eq!(
            ctx.get_field(narrow, MH_STATIC_OWNER),
            Value::Int(cell.as_u32() as i32)
        );
        assert_eq!(mh_minted_field_slot_with(&ctx, narrow, true), None);

        // Narrower still: nothing (and no write past the end).
        let tiny = ctx.alloc_object(cell, MH_STATIC_OWNER);
        mh_write_bound_field_carry(&ctx, tiny, carry);
    }

    #[test]
    fn the_owner_admits_bit_is_its_own() {
        let others = [
            FL_TYPE_INDEX_MASK << 16,
            0xFFFF,
            FL_LINKED,
            FL_STATIC,
            FL_SETTER,
            FL_VALUE_CHECK,
            FL_MINT_SLOT,
            0x7 << FL_LINKS_SHIFT,
        ];
        for bits in others {
            assert_eq!(
                bits & FL_OWNER_ADMITS,
                0,
                "{bits:#x} overlaps FL_OWNER_ADMITS"
            );
        }
        // A copy drops it with the link.
        let linked = FL_LINKED | FL_OWNER_ADMITS | FL_MINT_SLOT | 4;
        assert_eq!(field_link_state_kept_by_copy(linked), FL_MINT_SLOT | 4);
    }

    #[test]
    fn an_owner_link_admits_the_owner_and_its_subclasses_by_id() {
        let mut ctx = MockNativeContext::new();
        let base = ctx.ensure_class_initialized("p/Base").expect("p/Base");
        let sub = ctx.ensure_class_initialized("p/Sub").expect("p/Sub");
        let deep = ctx.ensure_class_initialized("p/Deep").expect("p/Deep");
        let other = ctx.ensure_class_initialized("p/Other").expect("p/Other");
        ctx.set_superclass(sub, base);
        ctx.set_superclass(deep, sub);
        // Another loader's `p/Base`: the name alone cannot tell them apart.
        let foreign = ctx.declare_second_copy("p/Base");
        let this = ctx.alloc_object(base, MH_SLOT_COUNT);
        ctx.set_field(this, MH_FIELD_LINK_VALUE, Value::Int(0));
        let on_base = ctx.alloc_object(base, 2);
        let on_sub = ctx.alloc_object(sub, 2);
        let on_deep = ctx.alloc_object(deep, 2);
        let on_other = ctx.alloc_object(other, 2);
        let on_foreign = ctx.alloc_object(foreign, 2);
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 1);
        let owner_raw = base.as_u32() as i32;
        let memo = |ctx: &MockNativeContext| ctx.get_field(this, MH_FIELD_LINK_VALUE);

        // A plain link: the exact class only (the pre-wave-65 rule).
        let plain = FL_LINKED | 1;
        assert!(field_link_receiver_admitted(
            &mut ctx, this, plain, owner_raw, on_base
        ));
        assert!(!field_link_receiver_admitted(
            &mut ctx, this, plain, owner_raw, on_sub
        ));
        assert_eq!(memo(&ctx), Value::Int(0), "a plain link remembers nothing");

        // An owner link: the owner, and each subclass, remembered last-seen.
        let owned = plain | FL_OWNER_ADMITS;
        assert!(field_link_receiver_admitted(
            &mut ctx, this, owned, owner_raw, on_base
        ));
        assert!(field_link_receiver_admitted(
            &mut ctx, this, owned, owner_raw, on_sub
        ));
        assert_eq!(memo(&ctx), Value::Int(sub.as_u32() as i32));
        assert!(field_link_receiver_admitted(
            &mut ctx, this, owned, owner_raw, on_deep
        ));
        assert_eq!(memo(&ctx), Value::Int(deep.as_u32() as i32));
        assert!(field_link_receiver_admitted(
            &mut ctx, this, owned, owner_raw, on_deep
        ));

        // Never an unrelated class, another loader's namesake, or an array;
        // none of them disturbs the memo.
        assert!(!field_link_receiver_admitted(
            &mut ctx, this, owned, owner_raw, on_other
        ));
        assert!(!field_link_receiver_admitted(
            &mut ctx, this, owned, owner_raw, on_foreign
        ));
        assert!(!field_link_receiver_admitted(
            &mut ctx, this, owned, owner_raw, arr
        ));
        assert_eq!(memo(&ctx), Value::Int(deep.as_u32() as i32));

        // A setter with a value check keeps its value class in that slot:
        // a subclass is admitted by the hierarchy, and the slot is untouched.
        let checked = owned | FL_SETTER | FL_VALUE_CHECK;
        ctx.set_field(this, MH_FIELD_LINK_VALUE, Value::Int(77));
        assert!(field_link_receiver_admitted(
            &mut ctx, this, checked, owner_raw, on_sub
        ));
        assert_eq!(memo(&ctx), Value::Int(77));
        assert!(!field_link_receiver_admitted(
            &mut ctx, this, checked, owner_raw, on_foreign
        ));
    }
}

/// Interpreter round i2 wave 66, lane L4: a bound field handle links too
/// (`CRATONVM_MH_FIELD_LINK_BOUND`), and the `bindTo` fast mint covers field
/// handles (`CRATONVM_MH_BINDTO_FAST_MINT_FIELDS`). The helpers take the
/// switches as parameters, and the fast path is driven on a hand-linked
/// handle; the end-to-end rows are
/// `tools/probes/interp/L4/L4W66BoundFieldLink.java`.
#[cfg(test)]
mod i66_l4_bound_field_link_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_bound_bit_is_its_own_and_the_type_index_still_fits() {
        let others = [
            FL_TYPE_INDEX_MASK << 16,
            0xFFFF,
            FL_OWNER_ADMITS,
            FL_LINKED,
            FL_STATIC,
            FL_SETTER,
            FL_VALUE_CHECK,
            FL_MINT_SLOT,
            0x7 << FL_LINKS_SHIFT,
        ];
        for bits in others {
            assert_eq!(bits & FL_BOUND, 0, "{bits:#x} overlaps FL_BOUND");
        }
        // `MethodHandle.type` is declared below the synthetic window.
        assert!(MH_BASE <= FL_TYPE_INDEX_MASK as usize);
        // A copy drops the bound link with the link, keeping the minted slot.
        let linked = FL_LINKED | FL_BOUND | FL_MINT_SLOT | (5 << 16) | 4;
        assert_eq!(field_link_state_kept_by_copy(linked), FL_MINT_SLOT | 4);
    }

    #[test]
    fn the_bound_shapes_are_the_unbound_links_one_parameter_shorter() {
        let g = |leaf: &[&str], lr: &str, decl: &[&str], dr: &str| {
            let (leaf, decl) = (strings(leaf), strings(decl));
            bound_field_link_value_type(false, &leaf, lr, &decl, dr).map(|v| v.map(str::to_string))
        };
        let s = |leaf: &[&str], decl: &[&str]| {
            let (leaf, decl) = (strings(leaf), strings(decl));
            bound_field_link_value_type(true, &leaf, "V", &decl, "V").map(|v| v.map(str::to_string))
        };
        // A getter: the same return, or a reference read as `Object`.
        assert_eq!(g(&["Lp/Cell;"], "I", &[], "I"), Some(None));
        assert_eq!(
            g(
                &["Lp/Cell;"],
                "Ljava/lang/String;",
                &[],
                "Ljava/lang/Object;"
            ),
            Some(None)
        );
        // A converted return, a static or misshapen descriptor: the door's.
        assert_eq!(g(&["Lp/Cell;"], "I", &[], "J"), None);
        assert_eq!(g(&["Lp/Cell;"], "I", &[], "Ljava/lang/Object;"), None);
        assert_eq!(g(&[], "I", &[], "I"), None);
        assert_eq!(g(&["Lp/Cell;"], "I", &["Lp/Cell;"], "I"), None);
        assert_eq!(g(&["I"], "I", &[], "I"), None);
        // A setter: a primitive value of the same type, a reference value
        // checked unless its leaf type is `Object`.
        assert_eq!(s(&["Lp/Cell;", "I"], &["I"]), Some(None));
        assert_eq!(s(&["Lp/Cell;", "I"], &["J"]), None);
        assert_eq!(
            s(&["Lp/Cell;", "Ljava/lang/Object;"], &["Ljava/lang/Object;"]),
            Some(None)
        );
        assert_eq!(
            s(&["Lp/Cell;", "Ljava/lang/String;"], &["Ljava/lang/String;"]),
            Some(Some("Ljava/lang/String;".to_string()))
        );
        assert_eq!(s(&["Lp/Cell;", "Ljava/lang/String;"], &["I"]), None);
        assert_eq!(s(&["I"], &[]), None);
        let (leaf, decl) = (strings(&["Lp/Cell;", "I"]), strings(&["I"]));
        assert_eq!(
            bound_field_link_value_type(true, &leaf, "I", &decl, "V"),
            None
        );
    }

    #[test]
    fn the_wide_copy_and_the_fast_mint_kinds_follow_their_switches() {
        let cell = ClassId::new(7);
        let owned = Some(BoundFieldCarry {
            owner: cell,
            link_state: 0,
        });
        let minted = Some(BoundFieldCarry {
            owner: cell,
            link_state: field_link_minted_state(2).unwrap(),
        });
        assert!(!bound_field_copy_needs_link_slots(None, true));
        assert!(!bound_field_copy_needs_link_slots(owned, false));
        assert!(bound_field_copy_needs_link_slots(owned, true));
        assert!(bound_field_copy_needs_link_slots(minted, false));

        for kind in [MH_KIND_STATIC, MH_KIND_VIRTUAL, MH_KIND_SPECIAL] {
            assert!(bindto_fast_mint_fills_kind(kind, false));
        }
        for kind in [MH_KIND_GETTER, MH_KIND_SETTER] {
            assert!(!bindto_fast_mint_fills_kind(kind, false));
            assert!(bindto_fast_mint_fills_kind(kind, true));
        }
        assert!(!bindto_fast_mint_fills_kind(MH_KIND_ARRAY_GET, true));
        assert!(!bindto_fast_mint_fills_kind(MH_KIND_CONSTANT, true));
    }

    #[test]
    fn a_hit_re_reads_the_carry_and_misses_on_another_owner() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let other = ctx.ensure_class_initialized("p/Other").expect("p/Other");
        let src =
            alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", "()I", MH_KIND_GETTER).unwrap();
        let mut answer = BindToMintAnswer {
            kind: MH_KIND_GETTER,
            type_desc: std::rc::Rc::from("()I"),
            collects: false,
            static_owner: None,
            recv_class: None,
            field_carry: Some(BoundFieldCarry {
                owner: cell,
                link_state: 0,
            }),
            prelink: None,
            alt: None,
        };
        // The source's minted slot (and only that, past a link) rides.
        let linked =
            field_link_minted_state(3).unwrap() | FL_LINKED | (5 << 16) | (1 << FL_LINKS_SHIFT);
        ctx.set_field(src, MH_FIELD_LINK, Value::Int(linked));
        assert!(bindto_fast_mint_refresh_carry_with(
            &ctx,
            src,
            &mut answer,
            Some(cell)
        ));
        assert_eq!(
            answer.field_carry,
            Some(BoundFieldCarry {
                owner: cell,
                link_state: field_link_minted_state(3).unwrap()
            })
        );
        // Another owner, or none: the slow path.
        assert!(!bindto_fast_mint_refresh_carry_with(
            &ctx,
            src,
            &mut answer,
            Some(other)
        ));
        assert!(!bindto_fast_mint_refresh_carry_with(
            &ctx,
            src,
            &mut answer,
            None
        ));
        // A direct line has nothing to refresh.
        let mut direct = BindToMintAnswer {
            field_carry: None,
            kind: MH_KIND_VIRTUAL,
            ..answer
        };
        assert!(bindto_fast_mint_refresh_carry_with(
            &ctx,
            src,
            &mut direct,
            None
        ));
        assert_eq!(direct.field_carry, None);
    }

    /// A bound getter / setter over `p/Cell`, hand-linked as
    /// [`mh_field_link_try_bound`] links it (the mock renders no `MethodType`,
    /// so `MH_CLASS`'s string stands in for the `type` object), with the
    /// bound receiver of class `cell` and slot 2.
    fn linked_bound(
        ctx: &mut MockNativeContext,
        kind: i32,
        receiver: ObjectRef,
        class_raw: i32,
        extra_flags: i32,
        site: &str,
    ) -> ObjectRef {
        let mh = alloc_method_handle(ctx, "p/Cell", "cellValueZ", "()I", kind).unwrap();
        assert!(matches!(ctx.get_field(mh, MH_VARARGS), Value::Int(0)));
        let Value::Object(Some(type_obj)) = ctx.get_field(mh, MH_CLASS) else {
            panic!("MH_CLASS holds a string");
        };
        ctx.set_field(mh, MH_BOUND, Value::Object(Some(receiver)));
        let site_id = field_link_site_id(site).expect("interned") as i32;
        ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
        ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(class_raw));
        ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
        ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(site_id));
        let state = FL_LINKED | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT) | extra_flags;
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(state));
        mh
    }

    #[test]
    fn a_bound_link_serves_its_receiver_and_nothing_else() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let stranger = ctx.ensure_class_initialized("p/Str").expect("p/Str");
        let r = ctx.alloc_object(cell, 4);
        ctx.set_field(r, 2, Value::Int(42));
        let raw = cell.as_u32() as i32;

        // A getter: the bound receiver's slot, no argument.
        let getter = linked_bound(&mut ctx, MH_KIND_GETTER, r, raw, FL_BOUND, "()I");
        let got = mh_field_link_fast(&mut ctx, getter, &[], Some("()I"));
        assert!(matches!(got, Some(Ok(Some(Value::Int(42))))));
        // An argument, or another call site: the door.
        assert!(mh_field_link_fast(&mut ctx, getter, &[Value::Int(1)], Some("()I")).is_none());
        if mh_strict_exact_signature() {
            assert!(mh_field_link_fast(&mut ctx, getter, &[], Some("()J")).is_none());
        }

        // A bound receiver of another class than the link names (a `String`):
        // the door, which casts and throws.
        let s = ctx.alloc_object(stranger, 4);
        ctx.set_field(s, 2, Value::Int(7));
        ctx.set_field(getter, MH_BOUND, Value::Object(Some(s)));
        assert!(mh_field_link_fast(&mut ctx, getter, &[], Some("()I")).is_none());

        // An UNBOUND link never serves a bound handle, nor a bound link an
        // unbound one.
        let plain = linked_bound(&mut ctx, MH_KIND_GETTER, r, raw, 0, "()I");
        assert!(mh_field_link_fast(&mut ctx, plain, &[], Some("()I")).is_none());
        assert!(
            mh_field_link_fast(&mut ctx, plain, &[Value::Object(Some(r))], Some("()I")).is_none()
        );
        let unbound = linked_bound(&mut ctx, MH_KIND_GETTER, r, raw, FL_BOUND, "()I");
        ctx.set_field(unbound, MH_BOUND, Value::Object(None));
        assert!(
            mh_field_link_fast(&mut ctx, unbound, &[Value::Object(Some(r))], Some("()I")).is_none()
        );
        // Never a bound static link.
        let st = linked_bound(
            &mut ctx,
            MH_KIND_GETTER,
            r,
            raw,
            FL_BOUND | FL_STATIC,
            "()I",
        );
        assert!(mh_field_link_fast(&mut ctx, st, &[], Some("()I")).is_none());

        // A setter: the value alone, stored in the bound receiver's slot.
        let setter = linked_bound(
            &mut ctx,
            MH_KIND_SETTER,
            r,
            raw,
            FL_BOUND | FL_SETTER,
            "(I)V",
        );
        let put = mh_field_link_fast(&mut ctx, setter, &[Value::Int(9)], Some("(I)V"));
        assert!(matches!(put, Some(Ok(Some(Value::Object(None))))));
        assert_eq!(ctx.get_field(r, 2), Value::Int(9));
        assert!(mh_field_link_fast(&mut ctx, setter, &[], Some("(I)V")).is_none());
        assert!(mh_field_link_fast(
            &mut ctx,
            setter,
            &[Value::Object(Some(r)), Value::Int(1)],
            Some("(I)V")
        )
        .is_none());
        assert_eq!(ctx.get_field(r, 2), Value::Int(9));
    }
}

/// Interpreter round i2 wave 67, lane L4: `bindTo` pre-links a bound field
/// copy (`CRATONVM_MH_BINDTO_PRELINK_FIELD`) and the `invoke` door serves a
/// linked field accessor at exactly its declared type
/// (`CRATONVM_MH_FIELD_LINK_SERVES_INVOKE`). Since the wave-67 triage the second is default on;
/// the tests drive the helpers below them either way; the end-to-end rows are
/// `tools/probes/interp/L4/L4W67BindToPrelink.java` and
/// `tools/probes/interp/L4/L4W67FieldLinkInvoke.java`.
#[cfg(test)]
mod i67_l4_prelink_and_invoke_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn the_switches_default_as_the_wave_67_triage_left_them() {
        // The process under test sets neither: the pre-link is opt-in
        // (`truthy_word`), serving `invoke` is default on since the wave-67
        // triage (`on_unless_zero`).
        if std::env::var_os("CRATONVM_MH_BINDTO_PRELINK_FIELD").is_none() {
            assert!(!mh_bindto_prelink_field_enabled());
        }
        if std::env::var_os("CRATONVM_MH_FIELD_LINK_SERVES_INVOKE").is_none() {
            assert!(mh_field_link_serves_invoke_enabled());
        }
    }

    #[test]
    fn bindto_judges_no_call_and_links_no_value_check() {
        let args = [Value::Int(3)];
        let invoke = BoundLinkCall::Invoke {
            extra: &args,
            call_site: Some("(I)V"),
        };
        // An `invokeExact` passes its arguments at the declared arity only.
        assert_eq!(
            bound_link_call_extra(invoke, None, 1).map(<[Value]>::len),
            Some(1)
        );
        assert!(bound_link_call_extra(invoke, None, 0).is_none());
        let empty = BoundLinkCall::Invoke {
            extra: &[],
            call_site: Some("()I"),
        };
        assert_eq!(
            bound_link_call_extra(empty, None, 0).map(<[Value]>::len),
            Some(0)
        );
        // `bindTo`: nothing to judge, so a getter or an unchecked setter links
        // with no arguments, and a reference setter that needs its value
        // checked is left to its first call.
        assert_eq!(
            bound_link_call_extra(BoundLinkCall::BindTo, None, 0).map(<[Value]>::len),
            Some(0)
        );
        assert_eq!(
            bound_link_call_extra(BoundLinkCall::BindTo, None, 1).map(<[Value]>::len),
            Some(0)
        );
        assert!(
            bound_link_call_extra(BoundLinkCall::BindTo, Some("Ljava/lang/String;"), 1).is_none()
        );
        // `invokeExact`'s reference setter still judges its one value.
        let s = [Value::Object(None)];
        let checked = BoundLinkCall::Invoke {
            extra: &s,
            call_site: None,
        };
        assert!(bound_link_call_extra(checked, Some("Ljava/lang/String;"), 1).is_some());
    }

    #[test]
    fn the_candidate_checks_kind_marking_and_relinks() {
        let mut ctx = MockNativeContext::new();
        if !mh_field_link_mode_admits(&ctx)
            || !mh_field_link_enabled()
            || crate::nbflags().dbg_mh_dispatch
        {
            return;
        }
        let getter =
            alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", "()I", MH_KIND_GETTER).unwrap();
        assert_eq!(
            field_link_candidate(&mut ctx, getter),
            Some(FieldLinkCandidate {
                setter: false,
                old: 0,
                links: 0
            })
        );
        let setter =
            alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", "(I)V", MH_KIND_SETTER).unwrap();
        assert_eq!(
            field_link_candidate(&mut ctx, setter).map(|c| c.setter),
            Some(true)
        );
        // Another kind, a marked handle, a handle out of relinks: none.
        let virt =
            alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", "()I", MH_KIND_GETTER).unwrap();
        ctx.set_field(virt, MH_KIND, Value::Int(MH_KIND_VIRTUAL));
        assert_eq!(field_link_candidate(&mut ctx, virt), None);
        mh_write_marking_bits(&ctx, setter, 1);
        assert_eq!(field_link_candidate(&mut ctx, setter), None);
        let spent = FL_MAX_LINKS << FL_LINKS_SHIFT;
        ctx.set_field(getter, MH_FIELD_LINK, Value::Int(spent));
        assert_eq!(field_link_candidate(&mut ctx, getter), None);
        let once = FL_LINKED | (1 << FL_LINKS_SHIFT) | 2;
        ctx.set_field(getter, MH_FIELD_LINK, Value::Int(once));
        assert_eq!(
            field_link_candidate(&mut ctx, getter),
            Some(FieldLinkCandidate {
                setter: false,
                old: once,
                links: 1
            })
        );
    }

    #[test]
    fn a_pre_link_of_a_copy_with_no_declared_type_writes_nothing() {
        // No `type` (an unknown signature): the shared admission stops at the
        // declared type, exactly where a first-call link would.
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        let mh = alloc_method_handle(
            &mut ctx,
            "p/Cell",
            "cellValueZ",
            "(Lp/Cell;)I",
            MH_KIND_GETTER,
        )
        .unwrap();
        ctx.set_field_by_name(mh, "type", Value::Object(None));
        ctx.set_field(mh, MH_BOUND, Value::Object(Some(r)));
        ctx.set_field(mh, MH_STATIC_OWNER, Value::Int(cell.as_u32() as i32));
        mh_field_link_try_bound(
            &mut ctx,
            mh,
            r,
            false,
            0,
            0,
            BoundLinkCall::BindTo,
            None,
            &mut None,
        );
        assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED == 0));
        mh_bindto_prelink_bound_field(&mut ctx, mh);
        assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED == 0));
    }

    /// A getter over `p/Cell` linked at `site` (unbound, receiver class `raw`,
    /// slot 2), as `mh_field_link_try` links it; `MH_CLASS`'s string stands
    /// in for the `type` object the mock does not render.
    fn linked_getter(ctx: &mut MockNativeContext, raw: i32, site: &str) -> ObjectRef {
        let mh = alloc_method_handle(ctx, "p/Cell", "cellValueZ", "(Lp/Cell;)I", MH_KIND_GETTER)
            .unwrap();
        let Value::Object(Some(type_obj)) = ctx.get_field(mh, MH_CLASS) else {
            panic!("MH_CLASS holds a string");
        };
        let site_id = field_link_site_id(site).expect("interned") as i32;
        ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
        ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(raw));
        ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
        ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(site_id));
        let state = FL_LINKED | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT);
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(state));
        mh
    }

    #[test]
    fn invoke_reaches_the_link_only_at_the_declared_type() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        ctx.set_field(r, 2, Value::Int(42));
        let getter = linked_getter(&mut ctx, cell.as_u32() as i32, "(Lp/Cell;)I");
        assert!(field_link_site_is(&ctx, getter, Some("(Lp/Cell;)I")));
        // An `Object`-typed receiver, another return, no site: an `asType`
        // the link must not skip -- the door.
        assert!(!field_link_site_is(
            &ctx,
            getter,
            Some("(Ljava/lang/Object;)I")
        ));
        assert!(!field_link_site_is(&ctx, getter, Some("(Lp/Cell;)J")));
        assert!(!field_link_site_is(&ctx, getter, None));
        // Unlinked (a copy keeps only the minted slot): the door.
        let unlinked = alloc_method_handle(
            &mut ctx,
            "p/Cell",
            "cellValueZ",
            "(Lp/Cell;)I",
            MH_KIND_GETTER,
        )
        .unwrap();
        assert!(!field_link_site_is(&ctx, unlinked, Some("(Lp/Cell;)I")));
        // The switch is off by default: `invoke` keeps the door.
        if !mh_field_link_serves_invoke_enabled() {
            let args = [Value::Object(Some(r))];
            assert!(
                mh_field_link_fast_for_invoke(&mut ctx, getter, &args, Some("(Lp/Cell;)I"))
                    .is_none()
            );
        }
    }
}

/// Interpreter round i2 wave 68, lane L4: `invoke` at a field accessor's
/// declared type links it (`CRATONVM_MH_INVOKE_LINKS_FIELD`), and a `bindTo`
/// fast-mint hit replays the slow path's bound field pre-link
/// (`CRATONVM_MH_BINDTO_FAST_MINT_PRELINK`). The first is default on since
/// the wave-68 triage, the second stays off; the helpers
/// take the switch as a parameter where it matters. The end-to-end rows are
/// `tools/probes/interp/L4/L4W68InvokeLinksField.java` and
/// `tools/probes/interp/L4/L4W68InvokeLinksBench.java`.
#[cfg(test)]
mod i68_l4_invoke_links_and_mint_prelink_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn the_switches_default_as_the_wave_68_triage_left_them() {
        // The process under test sets neither. The invoke link is default on
        // since the wave-68 triage (`on_unless_zero`); the mint prelink stays
        // `truthy_word`.
        if std::env::var_os("CRATONVM_MH_INVOKE_LINKS_FIELD").is_none() {
            assert!(mh_invoke_links_field_enabled());
        }
        if std::env::var_os("CRATONVM_MH_BINDTO_FAST_MINT_PRELINK").is_none() {
            assert!(!mh_bindto_fast_mint_prelink_enabled());
        }
    }

    #[test]
    fn invoke_is_invoke_exact_only_at_the_declared_type() {
        let decl = Some("(Lp/Cell;)I");
        assert!(invoke_site_is_declared(Some("(Lp/Cell;)I"), decl));
        // An `Object`-typed receiver (the `--compatible` String-receiver row),
        // a widened return, a boxed return: an `asType`, never linked.
        assert!(!invoke_site_is_declared(
            Some("(Ljava/lang/Object;)I"),
            decl
        ));
        assert!(!invoke_site_is_declared(Some("(Lp/Cell;)J"), decl));
        assert!(!invoke_site_is_declared(
            Some("(Lp/Cell;)Ljava/lang/Object;"),
            decl
        ));
        // No site, an empty site, an unknown declared type.
        assert!(!invoke_site_is_declared(None, decl));
        assert!(!invoke_site_is_declared(Some(""), Some("")));
        assert!(!invoke_site_is_declared(Some("(Lp/Cell;)I"), None));
    }

    #[test]
    fn invoke_links_nothing_without_a_declared_type() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        let mh = alloc_method_handle(
            &mut ctx,
            "p/Cell",
            "cellValueZ",
            "(Lp/Cell;)I",
            MH_KIND_GETTER,
        )
        .unwrap();
        ctx.set_field_by_name(mh, "type", Value::Object(None));
        assert!(!mh_declared_type_is_site(&mut ctx, mh, "(Lp/Cell;)I"));
        let args = [Value::Object(Some(r))];
        mh_field_link_try_for_invoke(&mut ctx, mh, &args, Some("(Lp/Cell;)I"));
        mh_field_link_try_for_invoke(&mut ctx, mh, &args, Some(""));
        mh_field_link_try_for_invoke(&mut ctx, mh, &args, None);
        assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED == 0));
    }

    /// The bound getter link `mh_field_link_try_bound` writes for slot 2 of a
    /// `p/Cell` receiver (`MH_CLASS`'s string stands in for `type`, which the
    /// mock does not render), relinked once from a carry of `0`.
    fn bound_link_state() -> i32 {
        FL_LINKED | FL_BOUND | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT)
    }

    /// A fresh bound getter copy over `p/Cell` bound to `receiver`, unlinked.
    fn bound_copy(ctx: &mut MockNativeContext, receiver: ObjectRef) -> ObjectRef {
        let mh = alloc_method_handle(ctx, "p/Cell", "cellValueZ", "()I", MH_KIND_GETTER).unwrap();
        ctx.set_field(mh, MH_BOUND, Value::Object(Some(receiver)));
        mh
    }

    #[test]
    fn the_line_remembers_only_a_plain_bound_link() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        let raw = cell.as_u32() as i32;
        let carry = BoundFieldCarry {
            owner: cell,
            link_state: 0,
        };
        let mh = bound_copy(&mut ctx, r);
        // Nothing linked: nothing to remember.
        assert_eq!(bindto_prelink_memo_of_with(&ctx, mh, carry, true), None);
        let Value::Object(Some(type_obj)) = ctx.get_field(mh, MH_CLASS) else {
            panic!("MH_CLASS holds a string");
        };
        let site_id = field_link_site_id("()I").expect("interned") as i32;
        ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
        ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(raw));
        ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
        ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(site_id));
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(bound_link_state()));
        let memo = BoundPrelinkMemo {
            carry_state: 0,
            state: bound_link_state(),
            class_raw: raw,
            site_id,
        };
        assert_eq!(
            bindto_prelink_memo_of_with(&ctx, mh, carry, true),
            Some(memo)
        );
        // Off: nothing.
        assert_eq!(bindto_prelink_memo_of_with(&ctx, mh, carry, false), None);
        // A value check, an unbound link, an owner link: re-derived per hit.
        for bad in [
            bound_link_state() | FL_VALUE_CHECK,
            bound_link_state() & !FL_BOUND,
            bound_link_state() | FL_OWNER_ADMITS,
        ] {
            ctx.set_field(mh, MH_FIELD_LINK, Value::Int(bad));
            assert_eq!(bindto_prelink_memo_of_with(&ctx, mh, carry, true), None);
        }
        // A link made under another `type` object.
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(bound_link_state()));
        ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(r)));
        assert_eq!(bindto_prelink_memo_of_with(&ctx, mh, carry, true), None);
    }

    #[test]
    fn a_hit_replays_the_link_for_the_remembered_class_only() {
        let mut ctx = MockNativeContext::new();
        if !mh_field_link_mode_admits(&ctx)
            || !mh_field_link_enabled()
            || crate::nbflags().dbg_mh_dispatch
        {
            return;
        }
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let stranger = ctx.ensure_class_initialized("p/Str").expect("p/Str");
        let r = ctx.alloc_object(cell, 4);
        ctx.set_field(r, 2, Value::Int(42));
        let raw = cell.as_u32() as i32;
        let carry = BoundFieldCarry {
            owner: cell,
            link_state: 0,
        };
        let site_id = field_link_site_id("()I").expect("interned") as i32;
        let memo = BoundPrelinkMemo {
            carry_state: 0,
            state: bound_link_state(),
            class_raw: raw,
            site_id,
        };

        // No memo, or off: not replayed, nothing written.
        let mh = bound_copy(&mut ctx, r);
        let recv = Value::Object(Some(r));
        assert!(!bindto_fast_mint_replay_prelink_with(
            &mut ctx, mh, recv, carry, None, true
        ));
        assert!(!bindto_fast_mint_replay_prelink_with(
            &mut ctx,
            mh,
            recv,
            carry,
            Some(memo),
            false
        ));
        assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED == 0));

        // Another carry than the link was derived from: re-derived.
        let minted = BoundFieldCarry {
            owner: cell,
            link_state: field_link_minted_state(2).unwrap(),
        };
        assert!(!bindto_fast_mint_replay_prelink_with(
            &mut ctx,
            mh,
            recv,
            minted,
            Some(memo),
            true
        ));

        // A receiver of another class (a `String`), or one that is not the
        // copy's bound value: re-derived.
        let s = ctx.alloc_object(stranger, 4);
        let other = bound_copy(&mut ctx, s);
        let srecv = Value::Object(Some(s));
        assert!(!bindto_fast_mint_replay_prelink_with(
            &mut ctx,
            other,
            srecv,
            carry,
            Some(memo),
            true
        ));
        assert!(
            matches!(ctx.get_field(other, MH_FIELD_LINK), Value::Int(st) if st & FL_LINKED == 0)
        );
        assert!(!bindto_fast_mint_replay_prelink_with(
            &mut ctx,
            mh,
            srecv,
            carry,
            Some(memo),
            true
        ));

        // The remembered class: the link is written, and it serves the copy.
        if crate::nbflags().dbg_mh_field_link {
            return;
        }
        assert!(bindto_fast_mint_replay_prelink_with(
            &mut ctx,
            mh,
            recv,
            carry,
            Some(memo),
            true
        ));
        assert_eq!(
            ctx.get_field(mh, MH_FIELD_LINK),
            Value::Int(bound_link_state())
        );
        assert_eq!(ctx.get_field(mh, MH_FIELD_LINK_CLASS), Value::Int(raw));
        let got = mh_field_link_fast(&mut ctx, mh, &[], Some("()I"));
        assert!(matches!(got, Some(Ok(Some(Value::Int(42))))));
    }
}

/// Interpreter round i2 wave 69, lane L4: the call that writes a field
/// accessor's link is served by it (`CRATONVM_MH_LINKING_CALL_SERVED`, default
/// off), and a `bindTo` fast-mint line keeps a second admitted class
/// (`CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES`, default off). The helpers take
/// the switch as a parameter; the end-to-end rows are
/// `tools/probes/interp/L4/L4W69LinkingCallServed.java` and
/// `tools/probes/interp/L4/L4W69LinkingCallBench.java`.
#[cfg(test)]
mod i69_l4_linking_call_served_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn the_switch_is_on_by_default() {
        if std::env::var_os("CRATONVM_MH_LINKING_CALL_SERVED").is_none() {
            assert!(mh_linking_call_served_enabled());
        }
    }

    #[test]
    fn only_a_written_link_counts() {
        let linked = FL_LINKED | 2 | (1 << FL_LINKS_SHIFT);
        // Nothing before, a link after: written.
        assert!(field_link_was_written(0, linked));
        // A relink raises the count.
        assert!(field_link_was_written(
            linked,
            FL_LINKED | 3 | (2 << FL_LINKS_SHIFT)
        ));
        // A minted slot only (no link), or the word unchanged: not written.
        let minted = field_link_minted_state(2).unwrap();
        assert!(!field_link_was_written(0, minted));
        assert!(!field_link_was_written(minted, minted));
        assert!(!field_link_was_written(linked, linked));
    }

    /// An UNBOUND instance getter over `p/Cell` slot 2, hand-linked as
    /// [`mh_field_link_try`] links it (the mock renders no `MethodType`, so
    /// `MH_CLASS`'s string stands in for the `type` object), receiver class
    /// `class_raw`, declared type `site`. Returns the handle and its link word.
    fn hand_linked_getter(
        ctx: &mut MockNativeContext,
        class_raw: i32,
        site: &str,
    ) -> (ObjectRef, i32) {
        let mh = alloc_method_handle(ctx, "p/Cell", "cellValueZ", "(Lp/Cell;)I", MH_KIND_GETTER)
            .unwrap();
        let Value::Object(Some(type_obj)) = ctx.get_field(mh, MH_CLASS) else {
            panic!("MH_CLASS holds a string");
        };
        let site_id = field_link_site_id(site).expect("interned") as i32;
        ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
        ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(class_raw));
        ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
        ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(site_id));
        let state = FL_LINKED | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT);
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(state));
        (mh, state)
    }

    #[test]
    fn the_linking_call_is_served_only_through_a_fresh_link_at_the_declared_type() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let stranger = ctx.ensure_class_initialized("p/Str").expect("p/Str");
        let r = ctx.alloc_object(cell, 4);
        ctx.set_field(r, 2, Value::Int(42));
        let raw = cell.as_u32() as i32;
        let site = Some("(Lp/Cell;)I");
        let (mh, state) = hand_linked_getter(&mut ctx, raw, "(Lp/Cell;)I");
        assert_eq!(field_link_state_word(&ctx, mh), state);
        let args = [Value::Object(Some(r))];
        let door = LinkingDoor::InvokeExact;

        // Off: the door.
        assert!(
            mh_field_link_serve_linking_call_with(&mut ctx, mh, &args, site, 0, door, false)
                .is_none()
        );
        // The attempt wrote nothing (the word it found is the word now): the door.
        assert!(mh_field_link_serve_linking_call_with(
            &mut ctx, mh, &args, site, state, door, true
        )
        .is_none());
        // Another call site than the declared type (a widened return, an
        // `Object` receiver): the door, whatever the strict switch says.
        for other in [
            Some("(Lp/Cell;)J"),
            Some("(Ljava/lang/Object;)I"),
            Some(""),
            None,
        ] {
            assert!(mh_field_link_serve_linking_call_with(
                &mut ctx, mh, &args, other, 0, door, true
            )
            .is_none());
        }
        // A receiver the link does not name, a null receiver, a wrong arity:
        // the door (its cast / message-less NPE / `WrongMethodTypeException`).
        let s = ctx.alloc_object(stranger, 4);
        for bad in [
            vec![Value::Object(Some(s))],
            vec![Value::Object(None)],
            vec![],
            vec![args[0], Value::Int(1)],
        ] {
            assert!(
                mh_field_link_serve_linking_call_with(&mut ctx, mh, &bad, site, 0, door, true)
                    .is_none()
            );
        }
        // Freshly written, at the declared type: the link's answer.
        let got = mh_field_link_serve_linking_call_with(&mut ctx, mh, &args, site, 0, door, true);
        assert!(matches!(got, Some(Ok(Some(Value::Int(42))))));
        // `invoke` reads the link only under `CRATONVM_MH_FIELD_LINK_SERVES_INVOKE`.
        let via_invoke = mh_field_link_serve_linking_call_with(
            &mut ctx,
            mh,
            &args,
            site,
            0,
            LinkingDoor::Invoke,
            true,
        );
        if mh_field_link_serves_invoke_enabled() {
            assert!(matches!(via_invoke, Some(Ok(Some(Value::Int(42))))));
        } else {
            assert!(via_invoke.is_none());
        }
    }

    #[test]
    fn an_unlinkable_handle_keeps_the_door() {
        // The mock renders no declared type, so the attempt links nothing and
        // the call is the door's whichever door it came through.
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        let mh = alloc_method_handle(
            &mut ctx,
            "p/Cell",
            "cellValueZ",
            "(Lp/Cell;)I",
            MH_KIND_GETTER,
        )
        .unwrap();
        ctx.set_field_by_name(mh, "type", Value::Object(None));
        let args = [Value::Object(Some(r))];
        for door in [LinkingDoor::InvokeExact, LinkingDoor::Invoke] {
            assert!(
                mh_field_link_try_and_serve(&mut ctx, mh, &args, Some("(Lp/Cell;)I"), door)
                    .is_none()
            );
        }
        assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED == 0));
    }

    // ---- `CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES` (wave 69) ----

    fn memo(class_raw: i32) -> BoundPrelinkMemo {
        BoundPrelinkMemo {
            carry_state: 0,
            state: FL_LINKED | FL_BOUND | 2,
            class_raw,
            site_id: 0,
        }
    }

    fn answer_for(class: Option<u32>, owner: ClassId) -> BindToMintAnswer {
        BindToMintAnswer {
            kind: MH_KIND_GETTER,
            type_desc: std::rc::Rc::from("()I"),
            collects: false,
            static_owner: None,
            recv_class: class,
            field_carry: Some(BoundFieldCarry {
                owner,
                link_state: 0,
            }),
            prelink: class.map(|c| memo(c as i32)),
            alt: None,
        }
    }

    #[test]
    fn the_two_class_switch_is_on_by_default() {
        if std::env::var_os("CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES").is_none() {
            assert!(
                mh_bindto_fast_mint_two_classes_enabled(),
                "default on since the wave-69 triage"
            );
        }
    }

    #[test]
    fn a_refill_for_another_class_keeps_the_first_as_the_second() {
        let owner = ClassId::new(7);
        let a = answer_for(Some(11), owner);
        let b = answer_for(Some(12), owner);
        // Off, no old line, or the same class: the new answer alone.
        assert_eq!(
            bindto_mint_answer_merged(Some(&a), b.clone(), false).alt,
            None
        );
        assert_eq!(bindto_mint_answer_merged(None, b.clone(), true).alt, None);
        assert_eq!(
            bindto_mint_answer_merged(Some(&b), b.clone(), true).alt,
            None
        );
        // On: B first, A kept with its own pre-link.
        let merged = bindto_mint_answer_merged(Some(&a), b.clone(), true);
        assert_eq!(merged.recv_class, Some(12));
        assert_eq!(merged.prelink, Some(memo(12)));
        assert_eq!(
            merged.alt,
            Some(BindToMintAlt {
                recv_class: 11,
                prelink: Some(memo(11))
            })
        );
        // A third class replaces the older of the two.
        let c = answer_for(Some(13), owner);
        let third = bindto_mint_answer_merged(Some(&merged), c, true);
        assert_eq!(third.recv_class, Some(13));
        assert_eq!(third.alt.map(|alt| alt.recv_class), Some(12));
        // A line that judges no class, or a new answer that judges none: alone.
        let any = answer_for(None, owner);
        assert_eq!(
            bindto_mint_answer_merged(Some(&any), b.clone(), true).alt,
            None
        );
        assert_eq!(
            bindto_mint_answer_merged(Some(&a), any.clone(), true).alt,
            None
        );
        // A shape disagreement (another carry owner, another kind): alone.
        let other_owner = answer_for(Some(11), ClassId::new(8));
        assert_eq!(
            bindto_mint_answer_merged(Some(&other_owner), b.clone(), true).alt,
            None
        );
        let setter = BindToMintAnswer {
            kind: MH_KIND_SETTER,
            ..answer_for(Some(11), owner)
        };
        assert_eq!(bindto_mint_answer_merged(Some(&setter), b, true).alt, None);
    }

    #[test]
    fn either_class_of_a_line_hits_with_its_own_pre_link() {
        let owner = ClassId::new(7);
        let merged = bindto_mint_answer_merged(
            Some(&answer_for(Some(11), owner)),
            answer_for(Some(12), owner),
            true,
        );
        let first = bindto_mint_answer_for_class(merged.clone(), 12).expect("the line's class");
        assert_eq!(first.prelink, Some(memo(12)));
        let second = bindto_mint_answer_for_class(merged.clone(), 11).expect("the second class");
        assert_eq!(second.recv_class, Some(11));
        assert_eq!(second.prelink, Some(memo(11)));
        assert_eq!(second.alt, None);
        assert!(bindto_mint_answer_for_class(merged, 13).is_none());
        // A line that judges no class admits any.
        assert!(bindto_mint_answer_for_class(answer_for(None, owner), 99).is_some());
        // Without a second class, only the line's.
        assert!(bindto_mint_answer_for_class(answer_for(Some(11), owner), 12).is_none());
    }
}

/// Interpreter round i2 wave 70, lane L4: `invoke`'s field link reads the
/// declared type once (`CRATONVM_MH_INVOKE_LINK_DECLARED_ONCE`, default off, a
/// pure refactor) and a link's derivation is memoised per source shape
/// (`CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO`, default off). The memo is driven
/// through its test VM override; the end-to-end rows are
/// `tools/probes/interp/L4/L4W70LinkDerivation.java` and
/// `tools/probes/interp/L4/L4W70LinkDerivationBench.java`.
#[cfg(test)]
mod i70_l4_link_derivation_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Forces the memo on for this thread (an empty memo); off and empty again
    /// when dropped.
    struct MemoOn;
    impl MemoOn {
        fn new() -> Self {
            FIELD_LINK_MEMO_TEST_VM.with(|v| v.set(Some(0x70_4C04)));
            FIELD_LINK_MEMO.with(|m| m.borrow_mut().iter_mut().for_each(|l| *l = None));
            MemoOn
        }
    }
    impl Drop for MemoOn {
        fn drop(&mut self) {
            FIELD_LINK_MEMO_TEST_VM.with(|v| v.set(None));
            FIELD_LINK_MEMO.with(|m| m.borrow_mut().iter_mut().for_each(|l| *l = None));
        }
    }

    const SITE: &str = "(Lp/Cell;)I";

    /// The link an owner-admitting derivation writes for slot 2 of a getter
    /// over `p/Cell` (`MH_CLASS`'s string stands in for the `type` object the
    /// mock does not render), first link from an empty word.
    fn owner_link_state() -> i32 {
        FL_LINKED | FL_OWNER_ADMITS | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT)
    }

    /// Write `state`'s link words on `mh` as a derivation would.
    fn hand_link(ctx: &mut MockNativeContext, mh: ObjectRef, state: i32, class_raw: i32) {
        let Value::Object(Some(type_obj)) = ctx.get_field(mh, MH_CLASS) else {
            panic!("MH_CLASS holds a string");
        };
        let site_id = field_link_site_id(SITE).expect("interned") as i32;
        ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
        ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(class_raw));
        ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
        ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(site_id));
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(state));
    }

    /// Back to unlinked (the word the candidate screen found: `0`).
    fn unlink(ctx: &mut MockNativeContext, mh: ObjectRef) {
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(0));
        ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(None));
        ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(0));
        ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(0));
    }

    const FRESH: FieldLinkCandidate = FieldLinkCandidate {
        setter: false,
        old: 0,
        links: 0,
    };

    #[test]
    fn the_switches_default() {
        if std::env::var_os("CRATONVM_MH_INVOKE_LINK_DECLARED_ONCE").is_none() {
            assert!(!mh_invoke_link_declared_once_enabled());
        }
        if std::env::var_os("CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO").is_none() {
            assert!(mh_field_link_derivation_memo_enabled());
        }
    }

    #[test]
    fn the_memo_keeps_only_owner_instance_links() {
        assert!(field_link_memo_keeps(owner_link_state()));
        assert!(field_link_memo_keeps(FL_LINKED | FL_BOUND | 2));
        assert!(field_link_memo_keeps(FL_LINKED | FL_MINT_SLOT | 2));
        // A static link, a by-name instance link, no link at all.
        assert!(!field_link_memo_keeps(
            FL_LINKED | FL_STATIC | FL_OWNER_ADMITS | 2
        ));
        assert!(!field_link_memo_keeps(FL_LINKED | 2));
        assert!(!field_link_memo_keeps(FL_MINT_SLOT | 2));
        // A bound receiver is one key word, whatever the receiver.
        assert_eq!(field_link_memo_bound_word(Value::Object(None)), 0);
        assert_ne!(field_link_memo_bound_word(Value::Int(5)), u64::MAX - 1);
    }

    #[test]
    fn off_the_memo_is_never_asked() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        let mh =
            alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", SITE, MH_KIND_GETTER).unwrap();
        let args = [Value::Object(Some(r))];
        // The mock reports VM identity `0`: no key, whatever the switch.
        let got = field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), true);
        assert_eq!(got, FieldLinkMemoReplay::Miss(None));
    }

    #[test]
    fn a_remembered_derivation_is_written_for_the_same_shape_and_class_only() {
        let _on = MemoOn::new();
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let stranger = ctx.ensure_class_initialized("p/Str").expect("p/Str");
        let r = ctx.alloc_object(cell, 4);
        let mh =
            alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", SITE, MH_KIND_GETTER).unwrap();
        unlink(&mut ctx, mh);
        let args = [Value::Object(Some(r))];
        let raw = cell.as_u32() as i32;

        // An empty memo: a miss with the key.
        let FieldLinkMemoReplay::Miss(Some(key)) =
            field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), false)
        else {
            panic!("a keyed miss");
        };
        // The derivation (by hand) writes an owner link; the fill remembers it.
        hand_link(&mut ctx, mh, owner_link_state(), raw);
        field_link_memo_fill_after(&mut ctx, mh, FRESH, Some(key), &args);
        let linked: Vec<Value> = (MH_FIELD_LINK..=MH_FIELD_LINK_SITE)
            .map(|i| ctx.get_field(mh, i))
            .collect();

        // The next linking call of the same shape and class writes the same words.
        unlink(&mut ctx, mh);
        let got = field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), false);
        assert_eq!(got, FieldLinkMemoReplay::Written);
        let replayed: Vec<Value> = (MH_FIELD_LINK..=MH_FIELD_LINK_SITE)
            .map(|i| ctx.get_field(mh, i))
            .collect();
        assert_eq!(format!("{linked:?}"), format!("{replayed:?}"));

        // A receiver of another class, a null receiver, another arity: derived.
        unlink(&mut ctx, mh);
        let s = ctx.alloc_object(stranger, 4);
        for bad in [
            vec![Value::Object(Some(s))],
            vec![Value::Object(None)],
            vec![],
            vec![args[0], Value::Int(1)],
        ] {
            let got = field_link_memo_replay(&mut ctx, mh, FRESH, &bad, Some(SITE), false);
            assert!(matches!(got, FieldLinkMemoReplay::Miss(Some(_))));
            assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(0)));
        }
        // Judged sites: only the interned declared type.
        for other in [
            Some("(Lp/Cell;)J"),
            Some("(Ljava/lang/Object;)I"),
            Some(""),
            None,
        ] {
            let got = field_link_memo_replay(&mut ctx, mh, FRESH, &args, other, true);
            assert!(matches!(got, FieldLinkMemoReplay::Miss(Some(_))));
        }
        assert_eq!(
            field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), true),
            FieldLinkMemoReplay::Written
        );
        // Another prior link word (a relink): another key.
        let relink = FieldLinkCandidate {
            setter: false,
            old: owner_link_state(),
            links: 1,
        };
        let got = field_link_memo_replay(&mut ctx, mh, relink, &args, Some(SITE), false);
        assert!(matches!(got, FieldLinkMemoReplay::Miss(Some(k)) if k != key));
        // A member slot rewritten in place (here `MH_CLASS`, which the mock's
        // link also uses as its `type`): another key, derived.
        unlink(&mut ctx, mh);
        let name = ctx.get_field(mh, MH_NAME);
        let class = ctx.get_field(mh, MH_CLASS);
        ctx.set_field(mh, MH_CLASS, name);
        let got = field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), false);
        assert!(matches!(got, FieldLinkMemoReplay::Miss(Some(_))));
        ctx.set_field(mh, MH_CLASS, class);
    }

    #[test]
    fn a_link_the_memo_does_not_keep_fills_nothing() {
        let _on = MemoOn::new();
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        let mh =
            alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", SITE, MH_KIND_GETTER).unwrap();
        unlink(&mut ctx, mh);
        let args = [Value::Object(Some(r))];
        let FieldLinkMemoReplay::Miss(Some(key)) =
            field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), false)
        else {
            panic!("a keyed miss");
        };
        // A by-name instance link (no owner bit), then nothing written at all.
        let by_name = FL_LINKED | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT);
        hand_link(&mut ctx, mh, by_name, cell.as_u32() as i32);
        field_link_memo_fill_after(&mut ctx, mh, FRESH, Some(key), &args);
        unlink(&mut ctx, mh);
        field_link_memo_fill_after(&mut ctx, mh, FRESH, Some(key), &args);
        let got = field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), false);
        assert!(matches!(got, FieldLinkMemoReplay::Miss(Some(_))));
    }

    #[test]
    fn invoke_links_nothing_without_a_declared_type_either_way() {
        // The mock renders no declared type: neither form of `invoke`'s link
        // writes anything, so the refactor declines exactly where wave 68 did.
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        let mh =
            alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", SITE, MH_KIND_GETTER).unwrap();
        ctx.set_field_by_name(mh, "type", Value::Object(None));
        let args = [Value::Object(Some(r))];
        for once in [false, true] {
            for site in [Some(SITE), Some(""), None] {
                mh_field_link_try_for_invoke_with(&mut ctx, mh, &args, site, once, &mut None);
            }
        }
        assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED == 0));
    }
}

/// Interpreter round i3 wave 72, lane L4 (`CRATONVM_MH_SITE_TYPE_TABLE`):
/// the site-type rows of the bound-`MethodType` table. The switch is set
/// explicitly in every test (it is read through a thread override). The
/// end-to-end rows are `tools/probes/interp/L4/L4W72SiteTypeTable.java` and
/// `tools/probes/interp/L4/L4W72JdkOnlyMintBench.java`.
#[cfg(test)]
mod i72_l4_site_type_table_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::flags::with_thread_overrides;

    const SWITCH: &str = "CRATONVM_MH_SITE_TYPE_TABLE";

    #[test]
    fn the_switch_is_on_unless_an_off_word() {
        for (word, on) in [
            (None, true),
            (Some("0"), false),
            (Some("off"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            with_thread_overrides(&[(SWITCH, word)], || {
                assert_eq!(mh_site_type_table_enabled(), on, "{word:?}");
            });
        }
    }

    #[test]
    fn a_key_is_made_only_for_a_built_in_loader_and_a_method_descriptor() {
        let desc = "()Ljava/lang/Object;";
        let keys: Vec<String> = (0..=2)
            .map(|l| mh_site_type_key(Some(l), desc).expect("built-in loader"))
            .collect();
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[1], keys[2]);
        for key in &keys {
            assert_ne!(key, desc, "never the descriptor-only row");
            assert!(!key.contains('\0'), "never a class-keyed row");
            assert!(key.ends_with(desc));
        }
        assert_eq!(mh_site_type_key(None, desc), None, "unknown loader");
        assert_eq!(mh_site_type_key(Some(3), desc), None, "a user loader");
        assert_eq!(mh_site_type_key(Some(-1), desc), None);
        assert_eq!(mh_site_type_key(Some(2), "Ljava/lang/Object;"), None);
        assert_eq!(mh_site_type_key(Some(2), ""), None);
        assert_eq!(mh_site_type_key(Some(2), "(\u{1})V"), None);
    }

    #[test]
    fn a_site_row_survives_a_class_unload_and_is_per_loader() {
        let mut ctx = MockNativeContext::new();
        let object = ctx
            .ensure_class_initialized("java/lang/Object")
            .expect("Object");
        // Any heap object stands in for the type: the table only stores it.
        let mt = ctx.get_class_mirror(object);
        let app = mh_site_type_key(Some(2), "()Ljava/lang/Object;").expect("key");
        let boot = mh_site_type_key(Some(0), "()Ljava/lang/Object;").expect("key");
        assert!(ctx.bound_method_type_get(&app).is_none());
        assert_eq!(ctx.bound_method_type_put(&app, mt), mt);
        assert_eq!(ctx.bound_method_type_get(&app), Some(mt));
        assert!(
            ctx.bound_method_type_get(&boot).is_none(),
            "another loader's row"
        );
        assert!(
            ctx.bound_method_type_get("()Ljava/lang/Object;").is_none(),
            "not the bound-type row"
        );
        assert_eq!(ctx.bound_method_types.forget_classes(&|_: u32| true), 0);
        assert_eq!(ctx.bound_method_type_get(&app), Some(mt));
    }

    #[test]
    fn only_built_in_classes_primitives_and_primitive_arrays_are_admitted() {
        let mut ctx = MockNativeContext::new();
        let object = ctx
            .ensure_class_initialized("java/lang/Object")
            .expect("Object");
        ctx.set_loader_id_override(object, 0);
        let boxed = ctx.ensure_class_initialized("app/Box").expect("app/Box");
        ctx.set_loader_id_override(boxed, 2);
        let user = ctx.ensure_class_initialized("u/Foo").expect("u/Foo");
        ctx.set_loader_id_override(user, 9);
        let unknown = ctx
            .ensure_class_initialized("u/Unknown")
            .expect("u/Unknown");
        let object_m = ctx.get_class_mirror(object);
        let box_m = ctx.get_class_mirror(boxed);
        let user_m = ctx.get_class_mirror(user);
        let unknown_m = ctx.get_class_mirror(unknown);
        let int_m = ctx.primitive_class_mirror("int");
        let void_m = ctx.primitive_class_mirror("void");
        // A primitive array's mirror is not read.
        let int_array_m = object_m;
        let ok = |ctx: &MockNativeContext, desc: &str, p: &[ObjectRef], r: ObjectRef| {
            mh_site_type_components_builtin(ctx, desc, p, r, 2)
        };
        assert!(ok(
            &ctx,
            "(Ljava/lang/Object;I)Ljava/lang/Object;",
            &[object_m, int_m],
            object_m
        ));
        assert!(ok(&ctx, "(Lapp/Box;)V", &[box_m], void_m));
        assert!(ok(&ctx, "()I", &[], int_m));
        assert!(ok(&ctx, "([I)V", &[int_array_m], void_m));
        assert!(
            !ok(&ctx, "(Lu/Foo;)V", &[user_m], void_m),
            "a user loader's class"
        );
        assert!(
            !ok(&ctx, "()Lu/Foo;", &[], user_m),
            "a user loader's return"
        );
        assert!(
            !ok(&ctx, "(Lu/Unknown;)V", &[unknown_m], void_m),
            "no known loader"
        );
        assert!(
            !ok(&ctx, "([Ljava/lang/Object;)V", &[object_m], void_m),
            "an array of a class"
        );
        assert!(!ok(&ctx, "(II)V", &[int_m], void_m), "arity mismatch");
        assert!(!ok(&ctx, "not-a-descriptor", &[], void_m));
        // A row under the boot loader's key names boot classes only; under
        // the platform loader's, boot and platform classes.
        let boot = |ctx: &MockNativeContext, desc: &str, p: &[ObjectRef], r: ObjectRef| {
            mh_site_type_components_builtin(ctx, desc, p, r, 0)
        };
        assert!(boot(
            &ctx,
            "(Ljava/lang/Object;I)Ljava/lang/Object;",
            &[object_m, int_m],
            object_m
        ));
        assert!(
            !boot(&ctx, "(Lapp/Box;)V", &[box_m], void_m),
            "an application class under loader 0"
        );
        assert!(!mh_site_type_components_builtin(
            &ctx,
            "(Lapp/Box;)V",
            &[box_m],
            void_m,
            1
        ));
    }
}

/// Interpreter round i3 wave 73, lane L4: the `MethodHandleNatives` field
/// memo (`CRATONVM_MHN_RESOLVE_MEMO`) and the real field accessor's record
/// (`CRATONVM_MH_REAL_FIELD_RECORD`). Every switch is set explicitly through a
/// thread override. The end-to-end rows are
/// `tools/probes/interp/L4/L4W73FieldNativesMemo.java` and
/// `tools/probes/interp/L4/L4W73JdkOnlyFieldBench.java`.
#[cfg(test)]
mod i73_l4_field_memo_and_record_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::flags::with_thread_overrides;

    #[test]
    fn both_switches_are_off_unless_a_truthy_word() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("off"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            with_thread_overrides(&[("CRATONVM_MHN_RESOLVE_MEMO", word)], || {
                assert_eq!(mhn_resolve_memo_enabled(), on, "{word:?}");
            });
            with_thread_overrides(&[("CRATONVM_MH_REAL_FIELD_RECORD", word)], || {
                assert_eq!(
                    mh_real_field_record_enabled(),
                    on || word.is_none(),
                    "{word:?}"
                );
            });
        }
    }

    #[test]
    fn the_memo_answers_only_its_own_key_kind_flags_and_vm() {
        let ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x7301);
        let epoch = ctx.gc_collection_count();
        let key = [0x1000, 0x2000, 0x3000];
        let answer = MhnMemoAnswer::Resolve {
            flags: Some(0x0104_0001),
            holder: None,
        };
        mhn_memo_put(&ctx, epoch, MhnMemoKind::Resolve, key, 0x0104_0000, answer);
        assert_eq!(
            mhn_memo_get(&ctx, MhnMemoKind::Resolve, key, 0x0104_0000),
            Some(answer)
        );
        assert_eq!(
            mhn_memo_get(&ctx, MhnMemoKind::Resolve, key, 0x0204_0000),
            None,
            "another requested kind"
        );
        assert_eq!(
            mhn_memo_get(&ctx, MhnMemoKind::StaticBase, key, 0x0104_0000),
            None,
            "another native"
        );
        assert_eq!(
            mhn_memo_get(
                &ctx,
                MhnMemoKind::Resolve,
                [0x1000, 0x2008, 0x3000],
                0x0104_0000
            ),
            None,
            "another name object"
        );
        let other = MockNativeContext::new();
        other.set_vm_identity(0x7302);
        assert_eq!(
            mhn_memo_get(&other, MhnMemoKind::Resolve, key, 0x0104_0000),
            None,
            "another VM"
        );
    }

    #[test]
    fn nothing_is_stored_across_a_collection_or_for_vm_zero() {
        let ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x7303);
        let key = [0x5000, 0x6000, 0];
        let stale_epoch = ctx.gc_collection_count().wrapping_add(1);
        mhn_memo_put(
            &ctx,
            stale_epoch,
            MhnMemoKind::ObjectOffset,
            key,
            0,
            MhnMemoAnswer::Offset(3),
        );
        assert_eq!(mhn_memo_get(&ctx, MhnMemoKind::ObjectOffset, key, 0), None);
        let mock = MockNativeContext::new();
        let epoch = mock.gc_collection_count();
        mhn_memo_put(
            &mock,
            epoch,
            MhnMemoKind::ObjectOffset,
            key,
            0,
            MhnMemoAnswer::Offset(3),
        );
        assert_eq!(
            mhn_memo_get(&mock, MhnMemoKind::ObjectOffset, key, 0),
            None,
            "vm 0"
        );
    }

    #[test]
    fn every_memo_slot_is_in_range() {
        for a in [0usize, 8, 0x1234_5678, usize::MAX] {
            for kind in [
                MhnMemoKind::Resolve,
                MhnMemoKind::ObjectOffset,
                MhnMemoKind::StaticOffset,
                MhnMemoKind::StaticBase,
            ] {
                assert!(mhn_memo_slot(kind, &[a, a.wrapping_mul(3), a ^ 0x55]) < MHN_MEMO_SLOTS);
            }
        }
    }
}

/// Interpreter round i3 wave 77, lane L4: the field-finder mint memo
/// (`CRATONVM_MH_FINDER_MINT_MEMO`, default on). Every switch is set
/// explicitly through a thread override. The end-to-end rows are
/// `tools/probes/interp/L4/L4W77FinderMint.java` (behaviour) and
/// `tools/probes/interp/L4/L4W77FinderMintBench.java`.
#[cfg(test)]
mod i77_l4_finder_mint_tests {
    use super::*;
    use cratonvm_types::compat::CompatibilityMode;
    use cratonvm_types::flags::with_thread_overrides;

    const SWITCH: &str = "CRATONVM_MH_FINDER_MINT_MEMO";

    #[test]
    fn the_switch_is_on_unless_an_off_word() {
        for (word, on) in [
            (None, true),
            (Some("0"), false),
            (Some("off"), false),
            (Some(""), true),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            with_thread_overrides(&[(SWITCH, word)], || {
                assert_eq!(mh_finder_mint_memo_enabled(), on, "{word:?}");
            });
        }
    }

    #[test]
    fn the_intrinsics_register_only_under_the_switch_in_a_jdk_only_vm_without_a_jit() {
        for (word, mode, jit, want) in [
            (Some("1"), CompatibilityMode::JdkOnly, false, true),
            (Some("0"), CompatibilityMode::JdkOnly, false, false),
            (Some("1"), CompatibilityMode::JdkOnly, true, false),
            (Some("1"), CompatibilityMode::Compatible, false, false),
        ] {
            // `CRATONVM_MH_COMPAT_FINDER_MINT` is on by default since the w109
            // triage; this test is about the `--jdk-only` admission, so the
            // `--compatible` rows pin it off.
            with_thread_overrides(
                &[(SWITCH, word), ("CRATONVM_MH_COMPAT_FINDER_MINT", Some("0"))],
                || {
                    let mut r = NativeMethodRegistry::new();
                    r.set_compatibility_mode(mode);
                    r.set_real_jdk(true);
                    r.set_jit_enabled(jit);
                    r.set_intrinsic_body_compiles(false);
                    // Wave 81: the `finder-mint` row is on by default; this test
                    // is about the no-JIT registration, so no row is listed.
                    r.set_intrinsic_body_compiles_rows("");
                    let before = r.current_category();
                    register_p63_method_handles_lookup(&mut r);
                    for finder in FieldFinder::ALL {
                        let kind =
                            r.kind_of(FINDER_MINT_LOOKUP, finder.method_name(), FINDER_MINT_DESC);
                        assert_eq!(
                            kind == Some(NativeKind::Intrinsic),
                            want,
                            "{word:?} {mode:?} jit={jit} {finder:?}: {kind:?}"
                        );
                    }
                    assert_eq!(r.current_category(), before, "{word:?} {mode:?} jit={jit}");
                },
            );
        }
    }

    /// Interpreter round i3 wave 80, lane L4: under
    /// `CRATONVM_INTRINSIC_BODY_COMPILES` a JIT `--jdk-only` VM registers the
    /// four finders too, each marked body-compilable; without it a JIT VM
    /// gets none and nothing is marked.
    #[test]
    fn the_body_compiles_switch_registers_and_marks_them_in_a_jit_vm() {
        for (body_compiles, jit, want, marked) in [
            (false, true, false, false),
            (true, true, true, true),
            (false, false, true, false),
            (true, false, true, true),
        ] {
            with_thread_overrides(&[(SWITCH, Some("1"))], || {
                let mut r = NativeMethodRegistry::new();
                r.set_compatibility_mode(CompatibilityMode::JdkOnly);
                r.set_real_jdk(true);
                r.set_jit_enabled(jit);
                r.set_intrinsic_body_compiles(body_compiles);
                // Wave 81: the global switch alone (the default lists `finder-mint`).
                r.set_intrinsic_body_compiles_rows("");
                register_p63_method_handles_lookup(&mut r);
                for finder in FieldFinder::ALL {
                    let m = finder.method_name();
                    let kind = r.kind_of(FINDER_MINT_LOOKUP, m, FINDER_MINT_DESC);
                    assert_eq!(
                        kind == Some(NativeKind::Intrinsic),
                        want,
                        "body_compiles={body_compiles} jit={jit} {finder:?}: {kind:?}"
                    );
                    assert_eq!(
                        r.is_body_compilable_intrinsic(FINDER_MINT_LOOKUP, m, FINDER_MINT_DESC),
                        marked,
                        "body_compiles={body_compiles} jit={jit} {finder:?}"
                    );
                }
            });
        }
    }

    /// Wave 80: `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS=finder-mint` gives a JIT
    /// `--jdk-only` VM the four finders (marked body-compilable) WITHOUT the
    /// `MethodType` intern memo, which the global switch would also register.
    #[test]
    fn the_finder_mint_row_adopts_the_body_compiles_path_alone() {
        with_thread_overrides(
            &[
                (SWITCH, Some("1")),
                ("CRATONVM_MH_METHOD_TYPE_INTERN_MEMO", Some("1")),
            ],
            || {
                for (rows, want) in [
                    ("", false),
                    ("finder-mint", true),
                    ("method-type-intern", false),
                ] {
                    let mut r = NativeMethodRegistry::new();
                    r.set_compatibility_mode(CompatibilityMode::JdkOnly);
                    r.set_real_jdk(true);
                    r.set_jit_enabled(true);
                    r.set_intrinsic_body_compiles(false);
                    r.set_intrinsic_body_compiles_rows(rows);
                    register_p63_method_handles_lookup(&mut r);
                    register_t4_method_handle_invoke(&mut r);
                    for finder in FieldFinder::ALL {
                        let m = finder.method_name();
                        assert_eq!(
                            r.kind_of(FINDER_MINT_LOOKUP, m, FINDER_MINT_DESC)
                                == Some(NativeKind::Intrinsic),
                            want,
                            "{rows:?} {finder:?}"
                        );
                        assert_eq!(
                            r.is_body_compilable_intrinsic(FINDER_MINT_LOOKUP, m, FINDER_MINT_DESC),
                            want,
                            "{rows:?} {finder:?}"
                        );
                    }
                    let intern = r.kind_of(MT_INTERN_CLASS, "makeImpl", MT_INTERN_DESC);
                    assert_eq!(
                        intern.is_some(),
                        rows == "method-type-intern",
                        "{rows:?}: the intern memo follows its own row only"
                    );
                }
            },
        );
    }

    /// Wave 81 (lane L4): a registry that does not set the rows reads them
    /// from the environment; with `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` unset
    /// that is `INTRINSIC_BODY_COMPILES_ROWS_DEFAULT`, and `=0` lists no row.
    /// Whatever the default, it never registers the intern memo in a JIT VM,
    /// and the finders follow it (no default pinned).
    #[test]
    fn an_unset_rows_variable_reads_the_default_rows_and_only_the_finders_follow_it() {
        let finder_default = cratonvm_native_api::registry::INTRINSIC_BODY_COMPILES_ROWS_DEFAULT
            .split(',')
            .any(|row| row.trim().eq_ignore_ascii_case(FINDER_MINT_BODY_ROW));
        for (rows, want) in [
            (None, finder_default),
            (Some("0"), false),
            (Some("none"), false),
        ] {
            with_thread_overrides(
                &[
                    (SWITCH, Some("1")),
                    ("CRATONVM_MH_METHOD_TYPE_INTERN_MEMO", Some("1")),
                    ("CRATONVM_INTRINSIC_BODY_COMPILES", Some("0")),
                    ("CRATONVM_INTRINSIC_BODY_COMPILES_ROWS", rows),
                ],
                || {
                    let mut r = NativeMethodRegistry::new();
                    r.set_compatibility_mode(CompatibilityMode::JdkOnly);
                    r.set_real_jdk(true);
                    r.set_jit_enabled(true);
                    register_p63_method_handles_lookup(&mut r);
                    register_t4_method_handle_invoke(&mut r);
                    for finder in FieldFinder::ALL {
                        let m = finder.method_name();
                        assert_eq!(
                            r.kind_of(FINDER_MINT_LOOKUP, m, FINDER_MINT_DESC)
                                == Some(NativeKind::Intrinsic),
                            want,
                            "{rows:?} {finder:?}"
                        );
                        assert_eq!(
                            r.is_body_compilable_intrinsic(FINDER_MINT_LOOKUP, m, FINDER_MINT_DESC),
                            want,
                            "{rows:?} {finder:?}"
                        );
                    }
                    assert_eq!(
                        r.kind_of(MT_INTERN_CLASS, "makeImpl", MT_INTERN_DESC),
                        None,
                        "{rows:?}: the intern memo stays no-JIT-only"
                    );
                    assert!(
                        !r.is_body_compilable_intrinsic(
                            MT_INTERN_CLASS,
                            "makeImpl",
                            MT_INTERN_DESC
                        ),
                        "{rows:?}"
                    );
                },
            );
        }
    }

    #[test]
    fn a_synthetic_jdk_never_gets_them() {
        with_thread_overrides(&[(SWITCH, Some("1"))], || {
            let mut r = NativeMethodRegistry::new();
            r.set_compatibility_mode(CompatibilityMode::JdkOnly);
            r.set_real_jdk(false);
            r.set_jit_enabled(false);
            assert!(!mh_finder_mint_memo_registers(&r));
        });
    }

    #[test]
    fn the_finders_name_their_methods_and_accessor_classes() {
        let names: Vec<&str> = FieldFinder::ALL.iter().map(|f| f.method_name()).collect();
        assert_eq!(
            names,
            [
                "findGetter",
                "findSetter",
                "findStaticGetter",
                "findStaticSetter"
            ]
        );
        for f in FieldFinder::ALL {
            let want = if f.is_static() {
                "java/lang/invoke/DirectMethodHandle$StaticAccessor"
            } else {
                "java/lang/invoke/DirectMethodHandle$Accessor"
            };
            assert_eq!(f.accessor_class(), want, "{f:?}");
        }
        let mut tags: Vec<u8> = FieldFinder::ALL.iter().map(|f| f.tag()).collect();
        tags.dedup();
        assert_eq!(tags.len(), 4);
    }

    #[test]
    fn every_key_word_moves_the_key_and_the_slot_stays_in_range() {
        let base = FinderMintKey {
            finder: 1,
            lookup_class: 0x1000,
            prev_lookup_class: 0,
            modes: 0x3f,
            refc: 0x2000,
            name: 0x3000,
            ty: 0x4000,
        };
        let variants = [
            FinderMintKey { finder: 2, ..base },
            FinderMintKey {
                lookup_class: 0x1010,
                ..base
            },
            FinderMintKey {
                prev_lookup_class: 0x1020,
                ..base
            },
            FinderMintKey {
                modes: 0x1f,
                ..base
            },
            FinderMintKey {
                refc: 0x2010,
                ..base
            },
            FinderMintKey {
                name: 0x3010,
                ..base
            },
            FinderMintKey { ty: 0x4010, ..base },
        ];
        assert!(finder_mint_slot(&base) < FINDER_MINT_MEMO_SLOTS);
        for v in variants {
            assert_ne!(v, base);
            assert!(finder_mint_slot(&v) < FINDER_MINT_MEMO_SLOTS);
        }
    }
}

/// Interpreter round i3 wave 82, lane L4: the converting adapter memo of
/// `asTypeUncached` (`CRATONVM_MH_REAL_AS_TYPE_MEMO`, default off). Switches
/// set explicitly; no default pinned. End to end:
/// `tools/probes/interp/L4/L4W82AsTypeMemo.java` and
/// `tools/probes/interp/L4/L4W72JdkOnlyMintBench.java`.
#[cfg(test)]
mod i82_l4_as_type_memo_tests {
    use super::*;
    use cratonvm_types::flags::with_thread_overrides;

    #[test]
    fn the_switch_reads_the_truthy_word_rule() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("1"), true),
            (Some("on"), true),
        ] {
            // Default on since the round-i4 wave-86 triage: the unset row is
            // the default and is not pinned.
            with_thread_overrides(&[("CRATONVM_MH_REAL_AS_TYPE_MEMO", word)], || {
                assert!(
                    mh_real_as_type_memo_enabled() == on || word.is_none(),
                    "{word:?}"
                );
            });
        }
    }

    #[test]
    fn only_direct_and_bound_species_receivers_are_admitted() {
        for (name, want) in [
            (
                "java/lang/invoke/DirectMethodHandle",
                Some(AsTypeReceiver::Direct),
            ),
            (
                "java/lang/invoke/DirectMethodHandle$Accessor",
                Some(AsTypeReceiver::Direct),
            ),
            (
                "java/lang/invoke/DirectMethodHandle$Special",
                Some(AsTypeReceiver::Direct),
            ),
            (
                "java/lang/invoke/BoundMethodHandle$Species_L",
                Some(AsTypeReceiver::Bound),
            ),
            (
                "java/lang/invoke/BoundMethodHandle$Species_LLI",
                Some(AsTypeReceiver::Bound),
            ),
            ("java/lang/invoke/BoundMethodHandle", None),
            ("java/lang/invoke/MethodHandleImpl$AsVarargsCollector", None),
            ("java/lang/invoke/MethodHandleImpl$WrappedMember", None),
            ("java/lang/invoke/MethodHandleImpl$CountingWrapper", None),
            ("java/lang/invoke/SimpleMethodHandle", None),
            ("java/lang/invoke/MethodHandle", None),
            ("java/lang/invoke/DirectMethodHandleX", None),
        ] {
            assert_eq!(as_type_memo_receiver_kind(name), want, "{name}");
        }
    }

    #[test]
    fn values_compare_by_identity_and_bits() {
        assert!(as_type_memo_same_value(Value::Int(3), Value::Int(3)));
        assert!(!as_type_memo_same_value(Value::Int(3), Value::Long(3)));
        assert!(as_type_memo_same_value(
            Value::Float(f32::NAN),
            Value::Float(f32::NAN)
        ));
        assert!(!as_type_memo_same_value(
            Value::Double(0.0),
            Value::Double(-0.0)
        ));
        assert!(as_type_memo_same_value(
            Value::Object(None),
            Value::Object(None)
        ));
    }

    /// Wave 84 (lane L4): the candidate's own receiver answering exactly the
    /// prediction mints for that receiver only; another receiver's agreeing
    /// answer promotes the key (from either state); any disagreeing answer
    /// refuses it; promoted and refused lines are final for the epoch.
    #[test]
    fn the_second_answer_promotes_self_promotes_or_refuses() {
        use AsTypeState as S;
        for (state, same, predicted, want) in [
            (S::Candidate, true, true, Some(S::PromotedSelf)),
            (S::Candidate, false, true, Some(S::Promoted)),
            (S::Candidate, true, false, Some(S::Refused)),
            (S::Candidate, false, false, Some(S::Refused)),
            (S::PromotedSelf, true, true, Some(S::PromotedSelf)),
            (S::PromotedSelf, false, true, Some(S::Promoted)),
            (S::PromotedSelf, false, false, Some(S::Refused)),
            (S::PromotedSelf, true, false, Some(S::Refused)),
            (S::Promoted, false, true, None),
            (S::Promoted, true, false, None),
            (S::Refused, false, true, None),
        ] {
            assert_eq!(
                as_type_memo_next_state(state, same, predicted),
                want,
                "{state:?} same={same} predicted={predicted}"
            );
        }
    }

    #[test]
    fn every_key_word_moves_the_key_and_the_slot_stays_in_range() {
        let base = AsTypeMemoKey {
            recv_class: 7,
            recv_type: 0x1000,
            recv_form: 0x2000,
            new_type: 0x3000,
        };
        let variants = [
            AsTypeMemoKey {
                recv_class: 8,
                ..base
            },
            AsTypeMemoKey {
                recv_type: 0x1010,
                ..base
            },
            AsTypeMemoKey {
                recv_form: 0x2010,
                ..base
            },
            AsTypeMemoKey {
                new_type: 0x3010,
                ..base
            },
        ];
        assert!(as_type_memo_slot(&base) < AS_TYPE_MEMO_SLOTS);
        for v in variants {
            assert_ne!(v, base);
            assert!(as_type_memo_slot(&v) < AS_TYPE_MEMO_SLOTS);
        }
    }
}

/// gcj j1/jt (page
/// `gci-i47w-oz47-jvmti-reflective-constructor-posts-no-vmobjectalloc`): with a
/// JVMTI `VMObjectAlloc` listener armed, both one-call constructor lanes
/// decline before reading the handle, so the handle's own form (whose
/// `Unsafe.allocateInstance` posts the event) runs. The positive control is
/// the probe row (`L1W63JvmtiEvents` `allocRows`, debug build).
#[cfg(test)]
mod gcj1jt_ctor_lane_vm_object_alloc_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeHeapAccess, NativeSystemAccess};
    use cratonvm_types::flags::with_thread_overrides;

    #[test]
    fn gcj1jt_an_armed_vm_object_alloc_listener_declines_both_constructor_lanes() {
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(ClassId::new(0), 1);
        ctx.jvmti_vm_object_alloc_armed_flag.set(true);
        let on = [
            ("CRATONVM_MH_REAL_CONSTRUCTOR_LANE", Some("1")),
            ("CRATONVM_MH_REAL_CTOR_WIDENING_SERVE", Some("1")),
        ];
        with_thread_overrides(&on, || {
            let site = Some("()Ljava/lang/Object;");
            assert!(mh_real_constructor_fast(&mut ctx, this, &[], site).is_none());
            assert!(mh_real_ctor_widening_serve(&mut ctx, this, &[], site).is_none());
        });
        // The trait default answers `false` (no agent: the lanes stay open).
        let fresh = MockNativeContext::new();
        assert!(!fresh.jvmti_vm_object_alloc_armed());
    }

    /// gcj j2/x1: the test above cannot fail -- its receiver is no
    /// `DirectMethodHandle$Constructor`, so both lanes decline at their class
    /// check with the armed test deleted. This pins the decline itself: in
    /// both lanes it precedes the receiver decode, and in the fast lane the
    /// memo hit too (a memo hit would instantiate with no event).
    #[test]
    fn gcj2x1_the_armed_decline_precedes_the_memo_and_the_receiver_decode() {
        let src: &'static str = LANG_INVOKE_SRC;
        for (f, memo) in [
            ("fn mh_real_constructor_fast(", true),
            ("fn mh_real_ctor_widening_serve(", false),
        ] {
            let at = src.find(f).expect(f);
            let end = src[at..].find("\n}\n").expect("fn end");
            let body = &src[at..at + end];
            let armed = body.find("ctx.jvmti_vm_object_alloc_armed()").expect("the decline");
            let decode = body.find("ctx.class_id_of_object(this)").expect("the decode");
            assert!(armed < decode, "{f}: decline before the decode");
            if memo {
                let hit = body.find("ctor_lane_memo_get(key)").expect("the memo");
                assert!(armed < hit, "{f}: decline before the memo hit");
            }
        }
    }
}

/// Interpreter round i4 wave 88, lane L4: `CRATONVM_MH_REAL_CTOR_WIDENING_SERVE`
/// (default off) parses as a `truthy_word` switch; no default is pinned.
#[cfg(test)]
mod i88_l4_ctor_widening_serve_tests {
    use cratonvm_types::flags::with_thread_overrides;

    #[test]
    fn the_switch_reads_the_truthy_word_rule() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("1"), true),
            (Some("on"), true),
        ] {
            with_thread_overrides(&[("CRATONVM_MH_REAL_CTOR_WIDENING_SERVE", word)], || {
                assert!(
                    crate::nbflags().mh_real_ctor_widening_serve == on || word.is_none(),
                    "{word:?}"
                );
            });
        }
    }
}

/// Interpreter round i3 wave 82, lane L4: the METHOD finder mint memo
/// (`CRATONVM_MH_METHOD_FINDER_MINT_MEMO`, default off). Every switch is set
/// explicitly through a thread override; no default is pinned. The
/// end-to-end rows are `tools/probes/interp/L4/L4W82MethodFinderMint.java`
/// (behaviour) and `tools/probes/interp/L4/L4W81MethodFinderBench.java`.
#[cfg(test)]
mod i82_l4_method_finder_mint_tests {
    use super::*;
    use cratonvm_types::compat::CompatibilityMode;
    use cratonvm_types::flags::with_thread_overrides;

    const SWITCH: &str = "CRATONVM_MH_METHOD_FINDER_MINT_MEMO";

    #[test]
    fn the_switch_reads_the_truthy_word_rule() {
        // Default on since the wave-83 triage; the unset and empty arms are
        // not pinned here.
        for (word, on) in [
            (Some("0"), false),
            (Some("off"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            with_thread_overrides(&[(SWITCH, word)], || {
                let read = mh_method_finder_mint_memo_enabled();
                // The unset row is the default: not pinned here.
                assert!(read == on || word.is_none(), "{word:?}");
            });
        }
    }

    fn registry(
        mode: CompatibilityMode,
        real_jdk: bool,
        jit: bool,
        rows: &str,
    ) -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        r.set_compatibility_mode(mode);
        r.set_real_jdk(real_jdk);
        r.set_jit_enabled(jit);
        r.set_intrinsic_body_compiles(false);
        r.set_intrinsic_body_compiles_rows(rows);
        r
    }

    #[test]
    fn the_intrinsics_register_only_under_the_switch_in_an_admitted_jdk_only_vm() {
        for (word, mode, real_jdk, jit, rows, want) in [
            (Some("1"), CompatibilityMode::JdkOnly, true, false, "", true),
            (
                Some("0"),
                CompatibilityMode::JdkOnly,
                true,
                false,
                "",
                false,
            ),
            (Some("1"), CompatibilityMode::JdkOnly, true, true, "", false),
            (
                Some("1"),
                CompatibilityMode::JdkOnly,
                true,
                true,
                "finder-mint",
                false,
            ),
            (
                Some("1"),
                CompatibilityMode::JdkOnly,
                true,
                true,
                "method-finder-mint",
                true,
            ),
            (
                Some("0"),
                CompatibilityMode::JdkOnly,
                true,
                true,
                "method-finder-mint",
                false,
            ),
            (
                Some("1"),
                CompatibilityMode::Compatible,
                true,
                false,
                "",
                false,
            ),
            (
                Some("1"),
                CompatibilityMode::JdkOnly,
                false,
                false,
                "",
                false,
            ),
        ] {
            // `CRATONVM_MH_COMPAT_FINDER_MINT` is on by default since the w109
            // triage; this test is about the `--jdk-only` admission, so the
            // `--compatible` rows pin it off.
            with_thread_overrides(
                &[(SWITCH, word), ("CRATONVM_MH_COMPAT_FINDER_MINT", Some("0"))],
                || {
                    let mut r = registry(mode, real_jdk, jit, rows);
                    let before = r.current_category();
                    register_p63_method_handles_lookup(&mut r);
                    for finder in MethodFinder::ALL {
                        let (m, d) = (finder.method_name(), finder.descriptor());
                        let kind = r.kind_of(FINDER_MINT_LOOKUP, m, d);
                        assert_eq!(
                            kind == Some(NativeKind::Intrinsic),
                            want,
                            "{word:?} {mode:?} real={real_jdk} jit={jit} rows={rows:?} {finder:?}: {kind:?}"
                        );
                        // Marked body-compilable exactly in a JIT VM that admits them.
                        assert_eq!(
                            r.is_body_compilable_intrinsic(FINDER_MINT_LOOKUP, m, d),
                            want && jit,
                            "{word:?} {mode:?} jit={jit} rows={rows:?} {finder:?}"
                        );
                    }
                    assert_eq!(r.current_category(), before, "{word:?} {mode:?} jit={jit}");
                },
            );
        }
    }

    /// Interpreter round i5 wave 109 (`CRATONVM_MH_COMPAT_FINDER_MINT`, default
    /// off): a `--compatible` VM over a real JDK with the `JdkInvoke` row
    /// (default on) registers the field and method finder `Intrinsic`s under
    /// the switch, through the family drop; without the switch, or without a
    /// real JDK, nothing of the finders is registered there.
    #[test]
    fn compatible_registers_the_finder_mints_only_under_the_compat_switch() {
        for (compat, real_jdk, want) in [
            (Some("1"), true, true),
            (Some("0"), true, false),
            // Unset: on by default since the w109 triage.
            (None, true, true),
            (Some("1"), false, false),
        ] {
            with_thread_overrides(
                &[
                    (SWITCH, Some("1")),
                    ("CRATONVM_MH_FINDER_MINT_MEMO", Some("1")),
                    ("CRATONVM_MH_COMPAT_FINDER_MINT", compat),
                ],
                || {
                    let mut r = registry(CompatibilityMode::Compatible, real_jdk, false, "");
                    if real_jdk
                        && !r.aligned(cratonvm_types::HotSpotAlignment::JdkInvoke)
                    {
                        // The row was switched off in this environment.
                        return;
                    }
                    let before = r.current_category();
                    register_p63_method_handles_lookup(&mut r);
                    for finder in MethodFinder::ALL {
                        let kind = r.kind_of(
                            FINDER_MINT_LOOKUP,
                            finder.method_name(),
                            finder.descriptor(),
                        );
                        assert_eq!(
                            kind == Some(NativeKind::Intrinsic),
                            want,
                            "{compat:?} real={real_jdk} {finder:?}: {kind:?}"
                        );
                    }
                    for finder in FieldFinder::ALL {
                        let kind =
                            r.kind_of(FINDER_MINT_LOOKUP, finder.method_name(), FINDER_MINT_DESC);
                        assert_eq!(
                            kind == Some(NativeKind::Intrinsic),
                            want,
                            "{compat:?} real={real_jdk} {finder:?}: {kind:?}"
                        );
                    }
                    assert_eq!(r.current_category(), before);
                },
            );
        }
    }

    /// The `method-finder-mint` row admits the method finders only: the field
    /// finders keep following their own `finder-mint` row.
    #[test]
    fn the_two_rows_are_separate_tokens() {
        with_thread_overrides(
            &[
                (SWITCH, Some("1")),
                ("CRATONVM_MH_FINDER_MINT_MEMO", Some("1")),
            ],
            || {
                for (rows, field, method) in [
                    ("method-finder-mint", false, true),
                    ("finder-mint", true, false),
                    ("finder-mint,method-finder-mint", true, true),
                ] {
                    let mut r = registry(CompatibilityMode::JdkOnly, true, true, rows);
                    register_p63_method_handles_lookup(&mut r);
                    for finder in FieldFinder::ALL {
                        let kind =
                            r.kind_of(FINDER_MINT_LOOKUP, finder.method_name(), FINDER_MINT_DESC);
                        assert_eq!(
                            kind == Some(NativeKind::Intrinsic),
                            field,
                            "{rows:?} {finder:?}"
                        );
                    }
                    for finder in MethodFinder::ALL {
                        let kind = r.kind_of(
                            FINDER_MINT_LOOKUP,
                            finder.method_name(),
                            finder.descriptor(),
                        );
                        assert_eq!(
                            kind == Some(NativeKind::Intrinsic),
                            method,
                            "{rows:?} {finder:?}"
                        );
                    }
                }
            },
        );
    }

    #[test]
    fn the_finders_name_their_methods_descriptors_and_tags() {
        let names: Vec<&str> = MethodFinder::ALL.iter().map(|f| f.method_name()).collect();
        assert_eq!(
            names,
            [
                "findVirtual",
                "findStatic",
                "findSpecial",
                "findConstructor",
                "unreflect"
            ]
        );
        assert_eq!(
            MethodFinder::Special.descriptor(),
            "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/Class;)\
             Ljava/lang/invoke/MethodHandle;"
        );
        for f in MethodFinder::ALL {
            assert!(
                f.descriptor().ends_with(")Ljava/lang/invoke/MethodHandle;"),
                "{f:?}"
            );
        }
        let mut tags: Vec<u8> = MethodFinder::ALL.iter().map(|f| f.tag()).collect();
        tags.extend(FieldFinder::ALL.iter().map(|f| f.tag()));
        tags.sort();
        tags.dedup();
        assert_eq!(
            tags.len(),
            9,
            "method and field finder tags are all distinct"
        );
    }

    #[test]
    fn only_the_four_direct_species_are_fileable() {
        for (name, want) in [
            (
                "java/lang/invoke/DirectMethodHandle",
                Some(DirectSpecies::Plain),
            ),
            (
                "java/lang/invoke/DirectMethodHandle$Special",
                Some(DirectSpecies::Special),
            ),
            (
                "java/lang/invoke/DirectMethodHandle$Interface",
                Some(DirectSpecies::Interface),
            ),
            (
                "java/lang/invoke/DirectMethodHandle$Constructor",
                Some(DirectSpecies::Constructor),
            ),
            ("java/lang/invoke/DirectMethodHandle$Accessor", None),
            ("java/lang/invoke/DirectMethodHandle$StaticAccessor", None),
            ("java/lang/invoke/MethodHandleImpl$AsVarargsCollector", None),
            ("java/lang/invoke/BoundMethodHandle$Species_L", None),
            ("java/lang/invoke/MethodHandleImpl$WrappedMember", None),
            ("java/lang/invoke/SimpleMethodHandle", None),
        ] {
            assert_eq!(DirectSpecies::of_class_name(name), want, "{name}");
        }
        for s in [
            DirectSpecies::Plain,
            DirectSpecies::Special,
            DirectSpecies::Interface,
            DirectSpecies::Constructor,
        ] {
            let words = s.words();
            for w in ["type", "form", "crackable"] {
                assert!(words.contains(&w), "{s:?} {w}");
            }
            // `member` / `initMethod` are fresh, never copied.
            assert!(
                !words.contains(&"member") && !words.contains(&"initMethod"),
                "{s:?}"
            );
        }
        assert!(method_finder_mint_is_polymorphic_class(Some(
            "java/lang/invoke/MethodHandle"
        )));
        assert!(method_finder_mint_is_polymorphic_class(Some(
            "java/lang/invoke/VarHandle"
        )));
        assert!(!method_finder_mint_is_polymorphic_class(Some(
            "java/lang/Object"
        )));
        assert!(!method_finder_mint_is_polymorphic_class(None));
    }

    #[test]
    fn every_key_word_moves_the_key_and_the_slot_stays_in_range() {
        let base = MethodFinderMintKey {
            finder: 11,
            lookup_class: 0x1000,
            prev_lookup_class: 0,
            modes: 0x3f,
            a: 0x2000,
            b: 0x3000,
            c: 0x4000,
            d: 0,
        };
        let variants = [
            MethodFinderMintKey { finder: 12, ..base },
            MethodFinderMintKey {
                lookup_class: 0x1010,
                ..base
            },
            MethodFinderMintKey {
                prev_lookup_class: 0x1020,
                ..base
            },
            MethodFinderMintKey {
                modes: 0x1f,
                ..base
            },
            MethodFinderMintKey { a: 0x2010, ..base },
            MethodFinderMintKey { b: 0x3010, ..base },
            MethodFinderMintKey { c: 0x4010, ..base },
            MethodFinderMintKey { d: 0x5000, ..base },
        ];
        assert!(method_finder_mint_slot(&base) < METHOD_FINDER_MINT_MEMO_SLOTS);
        for v in variants {
            assert_ne!(v, base);
            assert!(method_finder_mint_slot(&v) < METHOD_FINDER_MINT_MEMO_SLOTS);
        }
    }
}

/// Interpreter round i3 wave 74, lane L4: the `MethodType.makeImpl` intern memo
/// (`CRATONVM_MH_METHOD_TYPE_INTERN_MEMO`, default off). Every switch is set
/// explicitly through a thread override. The end-to-end rows are
/// `tools/probes/interp/L4/L4W74MethodTypeIntern.java` and
/// `tools/probes/interp/L4/L4W74FinderMintSplitBench.java`.
#[cfg(test)]
mod i74_l4_method_type_intern_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::flags::with_thread_overrides;

    const SWITCH: &str = "CRATONVM_MH_METHOD_TYPE_INTERN_MEMO";

    /// `makeImpl`'s arguments: a return-type object, a `Class[]` of `params`
    /// (`None` = a null element), `trusted`.
    fn make_args(
        ctx: &mut MockNativeContext,
        rtype: ObjectRef,
        params: &[Option<ObjectRef>],
    ) -> Vec<Value> {
        let arr = ctx.new_ref_array(ClassId::new(0), params.len());
        for (i, p) in params.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Object(*p));
        }
        vec![
            Value::Object(Some(rtype)),
            Value::Object(Some(arr)),
            Value::Int(1),
        ]
    }

    #[test]
    fn the_switch_is_off_unless_a_truthy_word() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("off"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            // Default on since the wave-75 triage: unset reads as on.
            with_thread_overrides(&[(SWITCH, word)], || {
                assert_eq!(
                    mh_method_type_intern_memo_enabled(),
                    on || word.is_none(),
                    "{word:?}"
                );
            });
        }
    }

    #[test]
    fn the_intrinsic_is_registered_only_under_the_switch_without_a_jit_and_the_category_is_restored(
    ) {
        // Wave 75: a VM with a JIT never gets it (the JIT arm measured worse).
        for (word, jit, expected) in [
            (Some("0"), false, None),
            (Some("1"), false, Some(NativeKind::Intrinsic)),
            (Some("0"), true, None),
            (Some("1"), true, None),
        ] {
            with_thread_overrides(&[(SWITCH, word)], || {
                let mut r = NativeMethodRegistry::new();
                r.set_jit_enabled(jit);
                r.set_intrinsic_body_compiles(false);
                let before = r.current_category();
                register_t4_method_handle_invoke(&mut r);
                assert_eq!(
                    r.kind_of(MT_INTERN_CLASS, "makeImpl", MT_INTERN_DESC),
                    expected,
                    "{word:?} jit={jit}"
                );
                assert_eq!(r.current_category(), before, "{word:?} jit={jit}");
            });
        }
    }

    /// Wave 80 (lane L4): under `CRATONVM_INTRINSIC_BODY_COMPILES` a JIT VM
    /// registers the memo too and marks `makeImpl` body-compilable.
    #[test]
    fn the_body_compiles_switch_registers_and_marks_make_impl_in_a_jit_vm() {
        for (body_compiles, jit, expected, marked) in [
            (false, true, None, false),
            (true, true, Some(NativeKind::Intrinsic), true),
            (false, false, Some(NativeKind::Intrinsic), false),
            (true, false, Some(NativeKind::Intrinsic), true),
        ] {
            with_thread_overrides(&[(SWITCH, Some("1"))], || {
                let mut r = NativeMethodRegistry::new();
                r.set_jit_enabled(jit);
                r.set_intrinsic_body_compiles(body_compiles);
                register_t4_method_handle_invoke(&mut r);
                assert_eq!(
                    r.kind_of(MT_INTERN_CLASS, "makeImpl", MT_INTERN_DESC),
                    expected,
                    "body_compiles={body_compiles} jit={jit}"
                );
                assert_eq!(
                    r.is_body_compilable_intrinsic(MT_INTERN_CLASS, "makeImpl", MT_INTERN_DESC),
                    marked,
                    "body_compiles={body_compiles} jit={jit}"
                );
            });
        }
    }

    #[test]
    fn an_unconfigured_registry_counts_as_a_jit_vm() {
        with_thread_overrides(&[(SWITCH, Some("1"))], || {
            let mut r = NativeMethodRegistry::new();
            r.set_intrinsic_body_compiles(false);
            assert!(r.jit_enabled());
            register_t4_method_handle_invoke(&mut r);
            assert_eq!(r.kind_of(MT_INTERN_CLASS, "makeImpl", MT_INTERN_DESC), None);
        });
    }

    fn key_n(n: usize) -> MtInternKey {
        MtInternKey {
            rtype: 0x10_0000 + 16 * n,
            len: 1,
            params: [0x20_0000 + 16 * n, 0, 0, 0, 0, 0],
        }
    }

    /// Keys whose sets collide in a memo of `lines` lines.
    fn colliding_keys(lines: usize, count: usize) -> Vec<MtInternKey> {
        let mut memo = MtInternMemo::new();
        memo.lines = vec![None; lines];
        let target = memo.set_of(mt_intern_hash(&key_n(0)));
        (0..100_000)
            .map(key_n)
            .filter(|k| memo.set_of(mt_intern_hash(k)) == target)
            .take(count)
            .collect()
    }

    #[test]
    fn two_keys_of_one_set_both_stay_and_a_third_evicts_the_older() {
        let mut ctx = MockNativeContext::new();
        let objs: Vec<ObjectRef> = (0..3)
            .map(|_| ctx.alloc_object(ClassId::new(0), 1))
            .collect();
        let keys = colliding_keys(MT_INTERN_MEMO_SLOTS, 3);
        assert_eq!(keys.len(), 3);
        let mut m = MtInternMemo::new();
        m.put(7, 1, keys[0], objs[0]);
        m.put(7, 1, keys[1], objs[1]);
        assert_eq!(m.get(7, 1, &keys[0]), Some(objs[0]), "both ways answer");
        assert_eq!(m.get(7, 1, &keys[1]), Some(objs[1]));
        // keys[1] is now the more recent: the third key evicts keys[0].
        m.put(7, 1, keys[2], objs[2]);
        assert_eq!(m.get(7, 1, &keys[0]), None);
        assert_eq!(m.get(7, 1, &keys[1]), Some(objs[1]));
        assert_eq!(m.get(7, 1, &keys[2]), Some(objs[2]));
        // The eviction is remembered: a miss on keys[0] is thrash.
        m.note_miss(7, &keys[0]);
        assert_eq!(m.thrash, 1);
        // A line of an earlier epoch is reused without an eviction.
        let before = m.victim_count;
        m.put(7, 2, keys[0], objs[0]);
        assert_eq!(m.victim_count, before);
        assert_eq!(m.get(7, 2, &keys[0]), Some(objs[0]));
        assert_eq!(m.get(7, 2, &keys[1]), None, "another epoch");
    }

    #[test]
    fn a_cyclic_working_set_grows_the_memo_and_then_hits() {
        let mut ctx = MockNativeContext::new();
        let mt = ctx.alloc_object(ClassId::new(0), 1);
        let keys: Vec<MtInternKey> = (0..1024).map(key_n).collect();
        let mut m = MtInternMemo::new();
        let mut misses_last_round = 0;
        for round in 0..16 {
            let mut misses = 0;
            for k in &keys {
                if m.get(9, 3, k).is_none() {
                    misses += 1;
                    m.note_miss(9, k);
                    m.put(9, 3, *k, mt);
                }
            }
            if round == 15 {
                misses_last_round = misses;
            }
        }
        assert_eq!(m.lines.len(), MT_INTERN_MEMO_MAX_SLOTS);
        // 1024 keys in 2048 two-way sets: ~9% of them share a set with two
        // others and miss every cycle.
        assert!(
            misses_last_round < keys.len() / 5,
            "{misses_last_round} misses of 1024"
        );
    }

    #[test]
    fn a_stream_of_new_types_does_not_grow_the_memo() {
        let mut ctx = MockNativeContext::new();
        let mt = ctx.alloc_object(ClassId::new(0), 1);
        let mut m = MtInternMemo::new();
        for n in 0..20_000 {
            let k = key_n(n);
            assert_eq!(m.get(9, 3, &k), None);
            m.note_miss(9, &k);
            m.put(9, 3, k, mt);
        }
        // Only victim-bit false positives count as thrash (at most ~12% of
        // the misses), never the half a growth needs.
        assert_eq!(m.lines.len(), MT_INTERN_MEMO_SLOTS);
    }

    #[test]
    fn another_vm_resets_the_memo() {
        let mut ctx = MockNativeContext::new();
        let mt = ctx.alloc_object(ClassId::new(0), 1);
        let mut m = MtInternMemo::new();
        m.put(1, 0, key_n(1), mt);
        m.put(2, 0, key_n(2), mt);
        assert_eq!(m.get(1, 0, &key_n(1)), None);
        assert_eq!(m.get(2, 0, &key_n(2)), Some(mt));
        assert_eq!(m.lines.len(), MT_INTERN_MEMO_SLOTS);
    }

    #[test]
    fn a_key_names_every_component_and_declines_what_the_body_must_answer() {
        let mut ctx = MockNativeContext::new();
        let rtype = ctx.alloc_object(ClassId::new(0), 1);
        let a = ctx.alloc_object(ClassId::new(0), 1);
        let b = ctx.alloc_object(ClassId::new(0), 1);
        let args = make_args(&mut ctx, rtype, &[Some(a), Some(b)]);
        let key = mt_intern_key(&ctx, &args).expect("two non-null parameters");
        assert_eq!(key.rtype, rtype.as_ptr() as usize);
        assert_eq!(key.len, 2);
        assert_eq!(key.params[0], a.as_ptr() as usize);
        assert_eq!(key.params[1], b.as_ptr() as usize);
        assert!(key.params[2..].iter().all(|&p| p == 0));
        // The order of the parameters is part of the key.
        let swapped = make_args(&mut ctx, rtype, &[Some(b), Some(a)]);
        assert_ne!(mt_intern_key(&ctx, &swapped), Some(key));
        // No parameters: a key of length 0.
        let none = make_args(&mut ctx, rtype, &[]);
        assert_eq!(mt_intern_key(&ctx, &none).map(|k| k.len), Some(0));
        // A null element (the body's NPE), too many parameters, a null return
        // type or array, a non-array: no key.
        let null_elem = make_args(&mut ctx, rtype, &[Some(a), None]);
        assert_eq!(mt_intern_key(&ctx, &null_elem), None);
        let many: Vec<Option<ObjectRef>> = (0..=MT_INTERN_MAX_PARAMS).map(|_| Some(a)).collect();
        let too_many = make_args(&mut ctx, rtype, &many);
        assert_eq!(mt_intern_key(&ctx, &too_many), None);
        let mut null_rtype = args.clone();
        null_rtype[0] = Value::Object(None);
        assert_eq!(mt_intern_key(&ctx, &null_rtype), None);
        let mut null_array = args.clone();
        null_array[1] = Value::Object(None);
        assert_eq!(mt_intern_key(&ctx, &null_array), None);
        let mut not_array = args.clone();
        not_array[1] = Value::Object(Some(a));
        assert_eq!(mt_intern_key(&ctx, &not_array), None);
    }

    #[test]
    fn the_memo_answers_only_its_own_key_vm_and_epoch() {
        let key = MtInternKey {
            rtype: 0x7400,
            len: 1,
            params: [0x7408, 0, 0, 0, 0, 0],
        };
        let mut ctx = MockNativeContext::new();
        let mt = ctx.alloc_object(ClassId::new(0), 1);
        mt_intern_memo_put(0x7401, 5, key, mt);
        assert_eq!(mt_intern_memo_get(0x7401, 5, &key), Some(mt));
        assert_eq!(mt_intern_memo_get(0x7402, 5, &key), None, "another VM");
        assert_eq!(mt_intern_memo_get(0x7401, 6, &key), None, "another epoch");
        let other = MtInternKey {
            params: [0x7410, 0, 0, 0, 0, 0],
            ..key
        };
        assert_eq!(
            mt_intern_memo_get(0x7401, 5, &other),
            None,
            "another parameter"
        );
        let longer = MtInternKey { len: 2, ..key };
        assert_eq!(
            mt_intern_memo_get(0x7401, 5, &longer),
            None,
            "another arity"
        );
        // VM identity 0 (the mocks' default) is never memoised.
        mt_intern_memo_put(0, 5, key, mt);
        assert_eq!(mt_intern_memo_get(0, 5, &key), None);
    }

    #[test]
    fn every_slot_is_in_range() {
        for a in [0usize, 8, 0x1234_5678, usize::MAX] {
            for len in 0..=MT_INTERN_MAX_PARAMS {
                let key = MtInternKey {
                    rtype: a,
                    len,
                    params: [a, a.wrapping_mul(3), a ^ 0x55, a.rotate_left(9), !a, a >> 4],
                };
                assert!(mt_intern_slot(&key) < MT_INTERN_MEMO_SLOTS);
            }
        }
    }

    #[test]
    fn a_hit_answers_the_filed_type_and_a_miss_runs_the_body_and_files_nothing_it_did_not_get() {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x7403);
        let rtype = ctx.alloc_object(ClassId::new(0), 1);
        let p = ctx.alloc_object(ClassId::new(0), 1);
        let mt = ctx.alloc_object(ClassId::new(0), 2);
        let args = make_args(&mut ctx, rtype, &[Some(p)]);
        // A miss: the mock's body answers nothing, which is returned as is and
        // not filed.
        let missed = mt_make_impl_intrinsic(&mut ctx, &args);
        assert!(matches!(missed, Ok(None)), "{missed:?}");
        let key = mt_intern_key(&ctx, &args).expect("key");
        let epoch = ctx.gc_collection_count();
        assert_eq!(mt_intern_memo_get(0x7403, epoch, &key), None);
        // A filed line answers its object without running the body.
        mt_intern_memo_put(0x7403, epoch, key, mt);
        match mt_make_impl_intrinsic(&mut ctx, &args) {
            Ok(Some(Value::Object(Some(answer)))) => assert_eq!(answer, mt),
            other => panic!("expected the filed type, got {other:?}"),
        }
        // Another parameter object: a miss again.
        let q = ctx.alloc_object(ClassId::new(0), 1);
        let other_args = make_args(&mut ctx, rtype, &[Some(q)]);
        assert!(matches!(
            mt_make_impl_intrinsic(&mut ctx, &other_args),
            Ok(None)
        ));
    }

    /// The body answers the transparent-frame count it ran under.
    fn gci_i18vp_observe(
        _ctx: &mut MockNativeContext,
        class: &str,
        name: &str,
        _desc: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        (class == MT_INTERN_CLASS && name == "makeImpl").then(|| {
            Ok(Some(Value::Int(
                cratonvm_native_api::continuation_pin::transparent_natives() as i32,
            )))
        })
    }

    /// The body yields (a park in the intern table that unmounts).
    fn gci_i18vp_yield(
        _ctx: &mut MockNativeContext,
        class: &str,
        _name: &str,
        _desc: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        if class != MT_INTERN_CLASS {
            return None;
        }
        assert_eq!(cratonvm_native_api::continuation_pin::transparent_natives(), 1);
        Some(Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::ContinuationYield {
                wake_after_nanos: 0,
            },
        )))
    }

    /// gci i18/vp: a memo miss re-enters `makeImpl` as a transparent tail
    /// call (`continuation_pin::tail_call` contract): one transparent frame
    /// for the body, its answer unchanged, a yield passed through with
    /// nothing filed, and the declaration withdrawn after either.
    #[test]
    fn gci_i18vp_the_make_impl_body_is_a_transparent_tail_call() {
        use cratonvm_native_api::continuation_pin::transparent_natives;
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x7405);
        let rtype = ctx.alloc_object(ClassId::new(0), 1);
        let p = ctx.alloc_object(ClassId::new(0), 1);
        let args = make_args(&mut ctx, rtype, &[Some(p)]);
        ctx.set_invoke_hook(gci_i18vp_observe);
        assert_eq!(mt_make_impl_intrinsic(&mut ctx, &args).ok(), Some(Some(Value::Int(1))));
        assert_eq!(transparent_natives(), 0, "withdrawn after the call");
        ctx.set_invoke_hook(gci_i18vp_yield);
        assert!(matches!(
            mt_make_impl_intrinsic(&mut ctx, &args),
            Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::ContinuationYield { .. }
            ))
        ));
        assert_eq!(transparent_natives(), 0, "withdrawn on the yield too");
        let key = mt_intern_key(&ctx, &args).expect("key");
        let epoch = ctx.gc_collection_count();
        assert_eq!(mt_intern_memo_get(0x7405, epoch, &key), None, "a yield files nothing");
    }
}

/// Interpreter round i3 wave 71, lane L4: a finder's field accessor shares its
/// member words (`CRATONVM_MH_FINDER_SHARED_MEMBER_WORDS`, default off) and the
/// linking call is served from what the link attempt admitted
/// (`CRATONVM_MH_LINKING_CALL_DIRECT_SERVE`, default off). Every switch is set
/// explicitly through a thread override. The end-to-end rows are
/// `tools/probes/interp/L4/L4W71FinderLinkServe.java` and
/// `tools/probes/interp/L4/L4W71LinkingCallBench.java`.
#[cfg(test)]
mod i71_l4_finder_words_and_direct_serve_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::flags::with_thread_overrides;

    const SITE: &str = "(Lp/Cell;)I";
    const FINDER: &str = "CRATONVM_MH_FINDER_SHARED_MEMBER_WORDS";
    const SERVED: &str = "CRATONVM_MH_LINKING_CALL_SERVED";
    const DIRECT: &str = "CRATONVM_MH_LINKING_CALL_DIRECT_SERVE";

    #[test]
    fn both_switches_are_on_unless_zero() {
        // Flipped on by the wave-71 triage (`on_unless_zero`); `=0` is the kill switch.
        for (word, on) in [
            (None, true),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            with_thread_overrides(&[(FINDER, word), (DIRECT, word)], || {
                assert_eq!(mh_finder_shared_member_words_enabled(), on, "{word:?}");
                assert_eq!(mh_linking_call_direct_serve_enabled(), on, "{word:?}");
            });
        }
    }

    #[test]
    fn only_field_accessors_share_their_member_words() {
        with_thread_overrides(&[(FINDER, Some("1"))], || {
            assert!(mh_finder_shares_member_words(MH_KIND_GETTER));
            assert!(mh_finder_shares_member_words(MH_KIND_SETTER));
            for kind in [
                MH_KIND_STATIC,
                MH_KIND_VIRTUAL,
                MH_KIND_SPECIAL,
                MH_KIND_CONSTRUCTOR,
            ] {
                assert!(!mh_finder_shares_member_words(kind), "kind {kind}");
            }
        });
        with_thread_overrides(&[(FINDER, Some("0"))], || {
            assert!(!mh_finder_shares_member_words(MH_KIND_GETTER));
            assert!(!mh_finder_shares_member_words(MH_KIND_SETTER));
        });
    }

    #[test]
    fn a_shared_mint_still_writes_every_member_slot() {
        // The mock pools nothing (`intern_string` is `create_string`) and
        // renders no `MethodType`: the mint must still hold the three texts.
        with_thread_overrides(&[(FINDER, Some("1"))], || {
            let mut ctx = MockNativeContext::new();
            let mh = alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", SITE, MH_KIND_GETTER)
                .unwrap();
            assert_eq!(mh_read_class(&ctx, mh).as_deref(), Some("p/Cell"));
            assert_eq!(mh_read_name(&ctx, mh).as_deref(), Some("cellValueZ"));
            assert_eq!(mh_read_desc(&ctx, mh).as_deref(), Some(SITE));
            assert_eq!(ctx.get_field(mh, MH_KIND), Value::Int(MH_KIND_GETTER));
            assert_eq!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(0));
        });
    }

    /// A handle over `p/Cell` slot 2 of `kind`, its `MH_BOUND` set to `bound`.
    fn handle(
        ctx: &mut MockNativeContext,
        kind: i32,
        desc: &str,
        bound: Option<ObjectRef>,
    ) -> ObjectRef {
        let mh = alloc_method_handle(ctx, "p/Cell", "cellValueZ", desc, kind).unwrap();
        ctx.set_field(mh, MH_BOUND, Value::Object(bound));
        mh
    }

    fn written(
        state: i32,
        class_raw: i32,
        receiver: Option<ObjectRef>,
        recv_raw: i32,
    ) -> FieldLinkWritten {
        FieldLinkWritten {
            state,
            class_raw,
            receiver,
            recv_raw,
            site_is_declared: true,
            site_id: -1,
        }
    }

    #[test]
    fn nothing_written_is_the_doors() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        let mh = handle(&mut ctx, MH_KIND_GETTER, SITE, None);
        let args = [Value::Object(Some(r))];
        assert!(mh_field_link_serve_written(&mut ctx, mh, &args, Some(SITE), None).is_none());
        // A note whose word is not a link (defensive).
        let raw = cell.as_u32() as i32;
        let w = written(2, raw, Some(r), raw);
        assert!(mh_field_link_serve_written(&mut ctx, mh, &args, Some(SITE), Some(w)).is_none());
    }

    #[test]
    fn an_unbound_getter_and_setter_are_served_at_the_declared_type_only() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        ctx.set_field(r, 2, Value::Int(42));
        let raw = cell.as_u32() as i32;
        let getter = handle(&mut ctx, MH_KIND_GETTER, SITE, None);
        let args = [Value::Object(Some(r))];
        let w = written(FL_LINKED | 2, raw, Some(r), raw);
        let got = mh_field_link_serve_written(&mut ctx, getter, &args, Some(SITE), Some(w));
        assert!(matches!(got, Some(Ok(Some(Value::Int(42))))));
        // A surplus argument, or none: the door's `WrongMethodTypeException`.
        for bad in [vec![args[0], Value::Int(1)], vec![]] {
            assert!(
                mh_field_link_serve_written(&mut ctx, getter, &bad, Some(SITE), Some(w)).is_none()
            );
        }
        // A site the writer did not judge: asked against the interned id.
        let site_id = field_link_site_id(SITE).expect("interned") as i32;
        let unjudged = FieldLinkWritten {
            site_is_declared: false,
            site_id,
            ..w
        };
        let got = mh_field_link_serve_written(&mut ctx, getter, &args, Some(SITE), Some(unjudged));
        assert!(matches!(got, Some(Ok(Some(Value::Int(42))))));
        for other in [
            Some("(Lp/Cell;)J"),
            Some("(Ljava/lang/Object;)I"),
            Some(""),
            None,
        ] {
            assert!(
                mh_field_link_serve_written(&mut ctx, getter, &args, other, Some(unjudged))
                    .is_none()
            );
        }
        // The setter stores and answers `void`.
        let setter = handle(&mut ctx, MH_KIND_SETTER, "(Lp/Cell;I)V", None);
        let w = written(FL_LINKED | FL_SETTER | 2, raw, Some(r), raw);
        let sargs = [Value::Object(Some(r)), Value::Int(7)];
        let got =
            mh_field_link_serve_written(&mut ctx, setter, &sargs, Some("(Lp/Cell;I)V"), Some(w));
        assert!(matches!(got, Some(Ok(Some(Value::Object(None))))));
        assert_eq!(ctx.get_field(r, 2), Value::Int(7));
        // A setter call without its value: the door.
        assert!(mh_field_link_serve_written(
            &mut ctx,
            setter,
            &args,
            Some("(Lp/Cell;I)V"),
            Some(w)
        )
        .is_none());
    }

    #[test]
    fn a_bound_link_reads_its_bound_receiver() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        ctx.set_field(r, 2, Value::Int(5));
        let raw = cell.as_u32() as i32;
        let mh = handle(&mut ctx, MH_KIND_GETTER, "()I", Some(r));
        let w = written(FL_LINKED | FL_BOUND | 2, raw, Some(r), raw);
        let got = mh_field_link_serve_written(&mut ctx, mh, &[], Some("()I"), Some(w));
        assert!(matches!(got, Some(Ok(Some(Value::Int(5))))));
        // The bound getter's surplus argument (the `invoke` row of
        // `i70-orch-compatible-bound-getter-invoke-ignores-an-extra-argument`).
        assert!(
            mh_field_link_serve_written(&mut ctx, mh, &[Value::Int(1)], Some("()I"), Some(w))
                .is_none()
        );
    }

    #[test]
    fn a_static_link_reads_and_writes_its_owner() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let raw = cell.as_u32() as i32;
        ctx.set_static_field(cell, 3, Value::Int(9));
        let getter = handle(&mut ctx, MH_KIND_GETTER, "()I", None);
        let w = written(FL_LINKED | FL_STATIC | 3, raw, None, 0);
        let got = mh_field_link_serve_written(&mut ctx, getter, &[], Some("()I"), Some(w));
        assert!(matches!(got, Some(Ok(Some(Value::Int(9))))));
        let setter = handle(&mut ctx, MH_KIND_SETTER, "(I)V", None);
        let w = written(FL_LINKED | FL_STATIC | FL_SETTER | 3, raw, None, 0);
        let got =
            mh_field_link_serve_written(&mut ctx, setter, &[Value::Int(11)], Some("(I)V"), Some(w));
        assert!(matches!(got, Some(Ok(Some(Value::Object(None))))));
        assert_eq!(ctx.get_static_field(cell, 3), Value::Int(11));
    }

    #[test]
    fn an_owner_link_remembers_a_subclass_receiver_as_the_fast_path_does() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let sub = ctx
            .ensure_class_initialized("p/SubCell")
            .expect("p/SubCell");
        let s = ctx.alloc_object(sub, 4);
        ctx.set_field(s, 2, Value::Int(3));
        let (raw, sub_raw) = (cell.as_u32() as i32, sub.as_u32() as i32);
        let mh = handle(&mut ctx, MH_KIND_GETTER, SITE, None);
        ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
        let w = written(FL_LINKED | FL_OWNER_ADMITS | 2, raw, Some(s), sub_raw);
        let got = mh_field_link_serve_written(
            &mut ctx,
            mh,
            &[Value::Object(Some(s))],
            Some(SITE),
            Some(w),
        );
        assert!(matches!(got, Some(Ok(Some(Value::Int(3))))));
        assert_eq!(ctx.get_field(mh, MH_FIELD_LINK_VALUE), Value::Int(sub_raw));
        // A value-checked owner link keeps its value class.
        ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(77));
        let checked = written(
            FL_LINKED | FL_OWNER_ADMITS | FL_VALUE_CHECK | 2,
            raw,
            Some(s),
            sub_raw,
        );
        let _ = mh_field_link_serve_written(
            &mut ctx,
            mh,
            &[Value::Object(Some(s))],
            Some(SITE),
            Some(checked),
        );
        assert_eq!(ctx.get_field(mh, MH_FIELD_LINK_VALUE), Value::Int(77));
    }

    #[test]
    fn a_memo_hit_notes_the_receiver_it_admitted() {
        FIELD_LINK_MEMO_TEST_VM.with(|v| v.set(Some(0x71_4C04)));
        FIELD_LINK_MEMO.with(|m| m.borrow_mut().iter_mut().for_each(|l| *l = None));
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        ctx.set_field(r, 2, Value::Int(42));
        let raw = cell.as_u32() as i32;
        let mh = handle(&mut ctx, MH_KIND_GETTER, SITE, None);
        let fresh = FieldLinkCandidate {
            setter: false,
            old: 0,
            links: 0,
        };
        let args = [Value::Object(Some(r))];
        let mut note = None;
        let FieldLinkMemoReplay::Miss(Some(key)) =
            field_link_memo_replay_noting(&mut ctx, mh, fresh, &args, Some(SITE), true, &mut note)
        else {
            panic!("a keyed miss");
        };
        assert_eq!(note, None);
        // The derivation (by hand) writes an owner link; the fill remembers it.
        let Value::Object(Some(type_obj)) = ctx.get_field(mh, MH_CLASS) else {
            panic!("MH_CLASS holds a string");
        };
        let state =
            FL_LINKED | FL_OWNER_ADMITS | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT);
        let site_id = field_link_site_id(SITE).expect("interned") as i32;
        ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
        ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(raw));
        ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
        ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(site_id));
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(state));
        field_link_memo_fill_after(&mut ctx, mh, fresh, Some(key), &args);
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(0));
        // The hit writes the words and notes the call it admitted.
        for judged in [true, false] {
            let mut note = None;
            let got = field_link_memo_replay_noting(
                &mut ctx,
                mh,
                fresh,
                &args,
                Some(SITE),
                judged,
                &mut note,
            );
            assert_eq!(got, FieldLinkMemoReplay::Written);
            let w = note.expect("noted");
            assert_eq!(
                (w.state, w.class_raw, w.receiver, w.recv_raw),
                (state, raw, Some(r), raw)
            );
            assert_eq!((w.site_is_declared, w.site_id), (judged, site_id));
            let served = mh_field_link_serve_written(&mut ctx, mh, &args, Some(SITE), Some(w));
            assert!(matches!(served, Some(Ok(Some(Value::Int(42))))));
            ctx.set_field(mh, MH_FIELD_LINK, Value::Int(0));
        }
        FIELD_LINK_MEMO_TEST_VM.with(|v| v.set(None));
        FIELD_LINK_MEMO.with(|m| m.borrow_mut().iter_mut().for_each(|l| *l = None));
    }

    #[test]
    fn an_unlinkable_handle_keeps_the_door_with_the_direct_serve_on() {
        // The mock renders no declared type: nothing is linked, nothing noted.
        with_thread_overrides(&[(SERVED, Some("1")), (DIRECT, Some("1"))], || {
            let mut ctx = MockNativeContext::new();
            let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
            let r = ctx.alloc_object(cell, 4);
            let mh = handle(&mut ctx, MH_KIND_GETTER, SITE, None);
            ctx.set_field_by_name(mh, "type", Value::Object(None));
            let args = [Value::Object(Some(r))];
            for door in [LinkingDoor::InvokeExact, LinkingDoor::Invoke] {
                assert!(
                    mh_field_link_try_and_serve(&mut ctx, mh, &args, Some(SITE), door).is_none()
                );
            }
            assert!(
                matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED == 0)
            );
        });
    }
}

/// Interpreter round i2 wave 64, lane L4: a `findSpecial` handle records its
/// dispatch class by id (`CRATONVM_MH_SPECIAL_BY_OWNER_ID`). The helpers take
/// the switch as a parameter; the end-to-end rows are
/// `tools/probes/interp/L4/L4W64SpecialByOwnerId.java`.
#[cfg(test)]
mod i64_l4_special_by_owner_id_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn the_dispatch_owner_is_refc_or_the_callers_superclass_by_id() {
        let mut ctx = MockNativeContext::new();
        let grand = ctx.ensure_class_initialized("p/Grand").expect("p/Grand");
        let mid = ctx.ensure_class_initialized("p/Mid").expect("p/Mid");
        let sub = ctx.ensure_class_initialized("p/Sub").expect("p/Sub");
        ctx.set_superclass(mid, grand);
        ctx.set_superclass(sub, mid);
        // A second loader's `p/Grand`: the name alone cannot tell them apart.
        let grand_b = ctx.declare_second_copy("p/Grand");
        // `refc` itself.
        assert_eq!(
            special_dispatch_owner(&ctx, "p/Grand", "p/Grand", Some(grand), Some(sub)),
            Some(grand)
        );
        assert_eq!(
            special_dispatch_owner(&ctx, "p/Grand", "p/Grand", Some(grand_b), Some(sub)),
            Some(grand_b),
            "the refc mirror's own class, not the name's"
        );
        // The restart at the caller's direct superclass.
        assert_eq!(
            special_dispatch_owner(&ctx, "p/Mid", "p/Grand", Some(grand), Some(sub)),
            Some(mid)
        );
        // Nothing guessed: no id, a name the id does not carry, no class.
        assert_eq!(
            special_dispatch_owner(&ctx, "p/Grand", "p/Grand", None, Some(sub)),
            None
        );
        assert_eq!(
            special_dispatch_owner(&ctx, "p/Mid", "p/Grand", Some(grand), None),
            None
        );
        assert_eq!(
            special_dispatch_owner(&ctx, "p/Other", "p/Grand", Some(grand), Some(sub)),
            None
        );
        assert_eq!(
            special_dispatch_owner(&ctx, "p/Grand", "p/Grand", Some(mid), Some(sub)),
            None
        );
        assert_eq!(
            special_dispatch_owner(&ctx, "", "", Some(grand), Some(sub)),
            None
        );
    }

    #[test]
    fn a_special_handle_records_and_reads_its_owner_only_when_on() {
        let mut ctx = MockNativeContext::new();
        let owner = ctx.ensure_class_initialized("p/Grand").expect("p/Grand");
        // Classless: the slot helpers read no name (and the mock builds no
        // receiver-typed `MethodType`).
        let mh = alloc_method_handle(&mut ctx, "", "m", "()I", MH_KIND_SPECIAL).unwrap();
        mh_record_special_owner_with(&ctx, mh, Some(owner), false);
        assert_eq!(
            mh_special_owner_with(&ctx, mh, true),
            None,
            "off: nothing recorded"
        );
        mh_record_special_owner_with(&ctx, mh, Some(owner), true);
        assert_eq!(mh_special_owner_with(&ctx, mh, true), Some(owner));
        assert_eq!(
            mh_special_owner_with(&ctx, mh, false),
            None,
            "off: nothing read"
        );
        // An `asType` copy keeps it (the member is the same).
        let copy = mh_clone_handle(&mut ctx, mh).expect("copy");
        assert_eq!(mh_special_owner_with(&ctx, copy, true), Some(owner));
        // A narrow handle: neither.
        let narrow = ctx.alloc_object(owner, MH_STATIC_OWNER);
        mh_record_special_owner_with(&ctx, narrow, Some(owner), true);
        assert_eq!(mh_special_owner_with(&ctx, narrow, true), None);
    }

    /// The orchestrator's fix of `L4W64SpecialByOwnerId` row
    /// `special-mid-on-mid`: the recorded `specialCaller` id casts the
    /// receiver, refusing another class with HotSpot's message.
    #[test]
    fn the_special_caller_id_casts_the_receiver() {
        let mut ctx = MockNativeContext::new();
        let mid = ctx.ensure_class_initialized("p/Mid").expect("p/Mid");
        let sub = ctx.ensure_class_initialized("p/Sub").expect("p/Sub");
        // Only a caller id that carries the caller's name is recorded.
        assert_eq!(
            special_caller_owner(&ctx, Some("p/Sub"), Some(sub)),
            Some(sub)
        );
        assert_eq!(special_caller_owner(&ctx, Some("p/Sub"), Some(mid)), None);
        assert_eq!(special_caller_owner(&ctx, Some("p/Sub"), None), None);
        assert_eq!(special_caller_owner(&ctx, None, Some(sub)), None);
        // Recorded and read in `MH_STATIC_SETTLED`, only when on.
        let mh = alloc_method_handle(&mut ctx, "", "m", "()I", MH_KIND_SPECIAL).unwrap();
        mh_record_special_caller_with(&ctx, mh, Some(sub), false);
        assert_eq!(mh_special_caller_with(&ctx, mh, true), None);
        mh_record_special_caller_with(&ctx, mh, Some(sub), true);
        assert_eq!(mh_special_caller_with(&ctx, mh, true), Some(sub));
        assert_eq!(mh_special_caller_with(&ctx, mh, false), None);
        // The verdict: the caller's own instance admitted, a `Mid` refused.
        let s = ctx.alloc_object(sub, 1);
        let m = ctx.alloc_object(mid, 1);
        assert!(matches!(
            special_caller_receiver_verdict(&mut ctx, s, sub),
            Some(None)
        ));
        match special_caller_receiver_verdict(&mut ctx, m, sub) {
            Some(Some(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { message },
            )))) => assert_eq!(message, "Cannot cast p.Mid to p.Sub"),
            other => panic!("expected the caller cast's refusal: {other:?}"),
        }
    }
}

/// Round 12 wave 7, lane mh5: the adapter chain of an `asType` /
/// `explicitCastArguments` copy (`MH_CAST_CHAIN`), one test per conversion row
/// of [`cast_chain_step`], and the door-side changes of the same wave. The
/// mock has no by-name hierarchy walk, so a POSITIVE reference mismatch is
/// always an array (an array is only an `Object`, `Cloneable` or
/// `Serializable`); a class value it cannot contradict must pass. The
/// end-to-end rows are `R12Hunt4MhAsTypeChain` and `R12Mh5CastChainRows`.
#[cfg(test)]
mod r12w7_mh5_cast_chain_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn object(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
        let o = match ctx.new_object(class) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("new_object {class}: {other:?}"),
        };
        ctx.set_field(o, 0, payload);
        Value::Object(Some(o))
    }

    fn int_array(ctx: &mut MockNativeContext) -> Value {
        Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)))
    }

    fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
        match build_method_type_from_descriptor(ctx, desc) {
            Ok(Some(mt)) => mt,
            other => panic!("MethodType {desc}: {other:?}"),
        }
    }

    /// Declare `MethodType`'s `rtype` / `ptypes` so `methodtype_to_descriptor`
    /// can read a type back (the mock resolves no field by name otherwise).
    fn readable_types(ctx: &mut MockNativeContext) {
        let probe = mt(ctx, "()V");
        let cid = ctx.class_id_of_object(probe);
        let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            cid,
            vec![
                field("rtype", "Ljava/lang/Class;", 0),
                field("ptypes", "[Ljava/lang/Class;", 1),
            ],
        );
    }

    fn setup() -> MockNativeContext {
        let mut ctx = MockNativeContext::new();
        for name in ["p/C", "p/Grand", "p/Mid", "p/Sub", "p/Greeter"] {
            let _ = ctx.ensure_class_initialized(name);
        }
        if let Ok(greeter) = ctx.ensure_class_initialized("p/Greeter") {
            ctx.set_is_interface(greeter, true);
        }
        readable_types(&mut ctx);
        ctx
    }

    fn handle(ctx: &mut MockNativeContext, class: &str, desc: &str, kind: i32) -> ObjectRef {
        match alloc_method_handle(ctx, class, "m", desc, kind) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc {class}: {e:?}"),
        }
    }

    fn stamp(ctx: &mut MockNativeContext, h: ObjectRef, desc: &str, explicit: bool) -> ObjectRef {
        let t = mt(ctx, desc);
        match mh_with_stamped_type(ctx, Value::Object(Some(h)), t, explicit) {
            Ok(Some(Value::Object(Some(copy)))) => copy,
            other => panic!("stamp {desc}: {other:?}"),
        }
    }

    fn cce_message(e: &MethodCallFailed) -> Option<String> {
        match e {
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { message, .. },
            )) => Some(message.clone()),
            _ => None,
        }
    }

    /// The class a `ClassCastException` names as its target (the value's own
    /// name, for an array, is the heap's to spell).
    fn cce_target(e: &MethodCallFailed) -> Option<String> {
        cce_message(e).and_then(|m| m.rsplit(" to ").next().map(str::to_string))
    }

    fn is_npe(e: &MethodCallFailed) -> bool {
        matches!(
            e,
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { .. }
            ))
        )
    }

    fn step(
        ctx: &mut MockNativeContext,
        v: ChainValue,
        from: &str,
        to: &str,
        explicit: bool,
    ) -> Result<ChainValue, MethodCallFailed> {
        cast_chain_step(ctx, v, from, to, explicit)
    }

    const OBJ: &str = "Ljava/lang/Object;";

    fn plain(v: Value) -> Option<ChainValue> {
        Some(ChainValue::Plain(v))
    }

    #[test]
    fn the_chain_text_round_trips_and_refuses_what_it_cannot_parse() {
        let chain = CastChain {
            bound_refusal: Some("Lp/Sub;".to_string()),
            links: vec![
                CastLink {
                    explicit: false,
                    desc: "(Lp/Sub;)I".to_string(),
                },
                // A class name may hold `)` and `;`-free punctuation.
                CastLink {
                    explicit: true,
                    desc: "(La)b;[[J)[Lx/Y;".to_string(),
                },
            ],
        };
        let text = render_cast_chain(&chain);
        assert_eq!(text, "!Lp/Sub;S(Lp/Sub;)IE(La)b;[[J)[Lx/Y;");
        assert_eq!(parse_cast_chain(&text), Some(chain));
        assert_eq!(parse_cast_chain("X()V"), None);
        assert_eq!(parse_cast_chain("S(I"), None);
        assert_eq!(parse_cast_chain("S()"), None);
        let bare = CastChain {
            bound_refusal: Some("I".to_string()),
            links: Vec::new(),
        };
        assert_eq!(parse_cast_chain("!I"), Some(bare));
        assert!(!CastChain::default().is_active());
    }

    /// Row: reference -> reference. `asType` casts (an array is not a `Sub`),
    /// a value the hierarchy cannot contradict and a `null` pass.
    #[test]
    fn row_reference_to_reference_casts() {
        let mut ctx = setup();
        let arr = int_array(&mut ctx);
        let s = object(&mut ctx, "java/lang/String", Value::Int(0));
        let r = step(&mut ctx, ChainValue::Plain(arr), OBJ, "Lp/Sub;", false);
        assert_eq!(
            r.as_ref().err().and_then(cce_target).as_deref(),
            Some("p.Sub")
        );
        assert_eq!(
            step(&mut ctx, ChainValue::Plain(s), OBJ, "Lp/Sub;", false).ok(),
            plain(s)
        );
        let null = ChainValue::Plain(Value::Object(None));
        assert_eq!(step(&mut ctx, null, OBJ, "Lp/Sub;", false).ok(), Some(null));
    }

    /// Row: reference -> INTERFACE. `asType` casts; `explicitCastArguments`
    /// passes the value "without a cast".
    #[test]
    fn row_reference_to_interface_is_uncast_only_under_an_explicit_link() {
        let mut ctx = setup();
        let arr = int_array(&mut ctx);
        assert!(step(&mut ctx, ChainValue::Plain(arr), OBJ, "Lp/Greeter;", false).is_err());
        assert_eq!(
            step(&mut ctx, ChainValue::Plain(arr), OBJ, "Lp/Greeter;", true).ok(),
            Some(ChainValue::Plain(arr))
        );
        // A class target still casts under an explicit link.
        assert!(step(&mut ctx, ChainValue::Plain(arr), OBJ, "Lp/Sub;", true).is_err());
    }

    /// Row: reference -> primitive under `asType`: a `null` is an NPE, a
    /// non-widening wrapper and a non-wrapper are a CCE, a widening wrapper
    /// converts.
    #[test]
    fn row_reference_to_primitive_unboxes_or_refuses() {
        let mut ctx = setup();
        let long = object(&mut ctx, "java/lang/Long", Value::Long(2));
        let short = object(&mut ctx, "java/lang/Short", Value::Int(2));
        let s = object(&mut ctx, "java/lang/String", Value::Int(0));
        let null = ChainValue::Plain(Value::Object(None));
        assert!(step(&mut ctx, null, OBJ, "I", false)
            .err()
            .is_some_and(|e| is_npe(&e)));
        assert!(step(&mut ctx, ChainValue::Plain(long), OBJ, "I", false)
            .err()
            .is_some_and(|e| cce_message(&e).is_some()));
        assert!(step(&mut ctx, ChainValue::Plain(s), OBJ, "I", false)
            .err()
            .is_some_and(|e| cce_message(&e).is_some()));
        assert_eq!(
            step(&mut ctx, ChainValue::Plain(short), OBJ, "I", false).ok(),
            Some(ChainValue::Plain(Value::Int(2)))
        );
    }

    /// Row: reference -> primitive under `explicitCastArguments`: a `null`
    /// is the zero, a `Long` narrows, a `Boolean` is 1.
    #[test]
    fn row_reference_to_primitive_explicitly_casts() {
        let mut ctx = setup();
        let long = object(&mut ctx, "java/lang/Long", Value::Long(0x1_0000_0005));
        let yes = object(&mut ctx, "java/lang/Boolean", Value::Int(1));
        let null = ChainValue::Plain(Value::Object(None));
        assert_eq!(
            step(&mut ctx, null, OBJ, "I", true).ok(),
            plain(Value::Int(0))
        );
        assert_eq!(
            step(&mut ctx, ChainValue::Plain(long), OBJ, "I", true).ok(),
            Some(ChainValue::Plain(Value::Int(5)))
        );
        assert_eq!(
            step(&mut ctx, ChainValue::Plain(yes), OBJ, "I", true).ok(),
            Some(ChainValue::Plain(Value::Int(1)))
        );
    }

    /// Row: primitive -> primitive. `asType` widens; an explicit link casts
    /// (a narrowing, `d2i`, and to `boolean` the low bit).
    #[test]
    fn row_primitive_to_primitive_widens_or_casts() {
        let mut ctx = setup();
        let three = ChainValue::Plain(Value::Int(3));
        assert_eq!(
            step(&mut ctx, three, "I", "J", false).ok(),
            plain(Value::Long(3))
        );
        let big = ChainValue::Plain(Value::Int(300));
        assert_eq!(
            step(&mut ctx, big, "I", "B", true).ok(),
            plain(Value::Int(44))
        );
        let six = ChainValue::Plain(Value::Int(6));
        assert_eq!(
            step(&mut ctx, six, "I", "Z", true).ok(),
            plain(Value::Int(0))
        );
        let d = ChainValue::Plain(Value::Double(-2.9));
        assert_eq!(
            step(&mut ctx, d, "D", "I", true).ok(),
            plain(Value::Int(-2))
        );
        // A wrapper object standing at a primitive position (the generic
        // door) is its primitive.
        let short = object(&mut ctx, "java/lang/Short", Value::Int(7));
        assert_eq!(
            step(&mut ctx, ChainValue::Plain(short), "S", "J", false).ok(),
            Some(ChainValue::Plain(Value::Long(7)))
        );
    }

    /// Row: primitive -> reference: boxed to the source's wrapper, whose
    /// supertypes are a closed table, then cast.
    #[test]
    fn row_primitive_to_reference_boxes_then_casts() {
        let mut ctx = setup();
        let five = ChainValue::Plain(Value::Int(5));
        assert_eq!(
            step(&mut ctx, five, "I", "Ljava/lang/Number;", false).ok(),
            Some(ChainValue::Boxed("I", Value::Int(5)))
        );
        let r = step(&mut ctx, five, "I", "Ljava/lang/Long;", true);
        assert_eq!(
            r.as_ref().err().and_then(cce_message).as_deref(),
            Some("Cannot cast java.lang.Integer to java.lang.Long")
        );
        assert!(step(&mut ctx, five, "I", "Lp/Greeter;", false).is_err());
        assert_eq!(
            step(&mut ctx, five, "I", "Lp/Greeter;", true).ok(),
            Some(ChainValue::Boxed("I", Value::Int(5)))
        );
    }

    /// Row: a box a previous link made, cast again and unboxed again.
    #[test]
    fn row_a_box_is_judged_by_its_wrapper_class() {
        let mut ctx = setup();
        let boxed = ChainValue::Boxed("I", Value::Int(5));
        assert_eq!(
            step(&mut ctx, boxed, OBJ, "Ljava/lang/Integer;", false).ok(),
            Some(boxed)
        );
        assert!(step(&mut ctx, boxed, OBJ, "Ljava/lang/String;", false).is_err());
        assert_eq!(
            step(&mut ctx, boxed, OBJ, "J", false).ok(),
            plain(Value::Long(5))
        );
        let r = step(&mut ctx, boxed, OBJ, "S", false);
        assert_eq!(
            r.as_ref().err().and_then(cce_message).as_deref(),
            Some("Cannot cast java.lang.Integer to java.lang.Short")
        );
        assert_eq!(
            step(&mut ctx, boxed, OBJ, "S", true).ok(),
            plain(Value::Int(5))
        );
    }

    /// What the judge cannot follow is never refused.
    #[test]
    fn what_cannot_be_followed_is_opaque() {
        let mut ctx = setup();
        let raw = ChainValue::Plain(Value::Int(1));
        assert_eq!(
            step(&mut ctx, raw, OBJ, "Lp/Sub;", false).ok(),
            Some(ChainValue::Opaque)
        );
        assert_eq!(
            step(&mut ctx, raw, "V", OBJ, false).ok(),
            Some(ChainValue::Opaque)
        );
        assert_eq!(
            step(&mut ctx, raw, "I", "V", false).ok(),
            Some(ChainValue::Opaque)
        );
        let null = ChainValue::Plain(Value::Object(None));
        assert_eq!(
            step(&mut ctx, null, "I", "J", false).ok(),
            Some(ChainValue::Opaque)
        );
        assert_eq!(
            step(&mut ctx, ChainValue::Opaque, OBJ, "I", false).ok(),
            Some(ChainValue::Opaque)
        );
    }

    /// `takesObj(Object).asType((Sub)int).asType((Object)int)`: the second
    /// stamp keeps the middle `Sub`, and the dispatch judge refuses a non-Sub
    /// there although outer and leaf are both `Object`.
    #[test]
    fn a_stamp_over_a_stamp_keeps_the_middle_type() {
        let mut ctx = setup();
        let h = handle(&mut ctx, "p/C", "(Ljava/lang/Object;)I", MH_KIND_STATIC);
        let c1 = stamp(&mut ctx, h, "(Lp/Sub;)I", false);
        assert_eq!(
            mh_cast_chain_text(&ctx, c1).as_deref(),
            Some("S(Ljava/lang/Object;)I")
        );
        assert_eq!(
            mh_marking_bits(&ctx, c1) & MH_MARK_CAST_CHAIN,
            0,
            "one stamp: no work"
        );
        let c2 = stamp(&mut ctx, c1, "(Ljava/lang/Object;)I", false);
        assert_eq!(
            mh_cast_chain_text(&ctx, c2).as_deref(),
            Some("S(Lp/Sub;)IS(Ljava/lang/Object;)I")
        );
        assert_ne!(mh_marking_bits(&ctx, c2) & MH_MARK_CAST_CHAIN, 0);
        // The receivers are untouched.
        assert_eq!(mh_cast_chain_text(&ctx, h), None);
        assert_eq!(
            mh_cast_chain_text(&ctx, c1).as_deref(),
            Some("S(Ljava/lang/Object;)I")
        );

        let chain =
            parse_cast_chain(&mh_cast_chain_text(&ctx, c2).unwrap_or_default()).unwrap_or_default();
        let outer = Some("(Ljava/lang/Object;)I");
        let arr = int_array(&mut ctx);
        let r = mh_cast_chain_judge_args(&mut ctx, c2, &chain, outer, &[arr]);
        assert_eq!(
            r.err().as_ref().and_then(cce_target).as_deref(),
            Some("p.Sub")
        );
        let s = object(&mut ctx, "java/lang/String", Value::Int(0));
        assert!(mh_cast_chain_judge_args(&mut ctx, c2, &chain, outer, &[s]).is_ok());
        // Another arity (a collector's) is not judged.
        assert!(mh_cast_chain_judge_args(&mut ctx, c2, &chain, outer, &[arr, arr]).is_ok());
    }

    /// `twiceLong.asType((int)long).explicitCastArguments((long)long)`: the
    /// explicit middle link narrows, so the leaf is handed `(long)(int) x`;
    /// a chain that changes nothing hands the arguments on as passed.
    #[test]
    fn a_middle_primitive_conversion_reaches_the_leaf() {
        let mut ctx = setup();
        let h = handle(&mut ctx, "p/C", "(J)J", MH_KIND_STATIC);
        let narrowed = stamp(&mut ctx, h, "(I)J", false);
        let c = stamp(&mut ctx, narrowed, "(J)J", true);
        assert_eq!(mh_cast_chain_text(&ctx, c).as_deref(), Some("E(I)JS(J)J"));
        assert!(
            !mh_is_explicit_cast(&ctx, c),
            "the innermost link is asType's"
        );
        let chain = parse_cast_chain("E(I)JS(J)J").unwrap_or_default();
        let wide = [Value::Long(0x1_0000_0005)];
        let r = mh_cast_chain_judge_args(&mut ctx, c, &chain, Some("(J)J"), &wide);
        assert_eq!(r.ok(), Some(Some(vec![Value::Long(5)])));
        let small = [Value::Long(5)];
        let r = mh_cast_chain_judge_args(&mut ctx, c, &chain, Some("(J)J"), &small);
        assert_eq!(r.ok(), Some(None));
    }

    /// `retObj.asType((Object)Integer).asType((Object)Object)`: the middle
    /// RETURN type is judged on the way out.
    #[test]
    fn the_middle_return_type_is_judged() {
        let mut ctx = setup();
        let chain = CastChain {
            bound_refusal: None,
            links: vec![
                CastLink {
                    explicit: false,
                    desc: "(Ljava/lang/Object;)Ljava/lang/Integer;".to_string(),
                },
                CastLink {
                    explicit: false,
                    desc: "(Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
                },
            ],
        };
        let outer = Some("(Ljava/lang/Object;)Ljava/lang/Object;");
        let arr = int_array(&mut ctx);
        let r = mh_cast_chain_judge_return(&mut ctx, &chain, outer, Ok(Some(arr)));
        assert_eq!(
            r.err().as_ref().and_then(cce_target).as_deref(),
            Some("java.lang.Integer")
        );
        let int = object(&mut ctx, "java/lang/Integer", Value::Int(4));
        let passed = mh_cast_chain_judge_return(&mut ctx, &chain, outer, Ok(Some(int)));
        assert_eq!(passed.ok(), Some(Some(int)));
        let void = mh_cast_chain_judge_return(&mut ctx, &chain, outer, Ok(None));
        assert_eq!(void.ok(), Some(None));
    }

    /// The explicit marking: a FIRST explicit stamp marks the copy (the leaf's
    /// conversion); an explicit stamp over a stamp is a middle link and keeps
    /// the receiver's marking; with the switch off every explicit copy is
    /// marked and nothing is recorded, as before.
    #[test]
    fn the_explicit_marking_follows_the_innermost_stamp() {
        let mut ctx = setup();
        let h = handle(&mut ctx, "p/C", "(I)I", MH_KIND_STATIC);
        let e1 = stamp(&mut ctx, h, "(J)I", true);
        assert!(mh_is_explicit_cast(&ctx, e1));
        assert_eq!(mh_cast_chain_text(&ctx, e1).as_deref(), Some("E(I)I"));
        let s2 = stamp(&mut ctx, e1, "(S)I", false);
        assert!(mh_is_explicit_cast(&ctx, s2), "inherited from the receiver");
        assert_eq!(mh_cast_chain_text(&ctx, s2).as_deref(), Some("S(J)IE(I)I"));

        let a1 = stamp(&mut ctx, h, "(Ljava/lang/Integer;)I", false);
        let e2 = stamp(&mut ctx, a1, "(Ljava/lang/Object;)I", true);
        assert!(
            !mh_is_explicit_cast(&ctx, e2),
            "the innermost link is asType's"
        );
        assert_eq!(
            mh_cast_chain_text(&ctx, e2).as_deref(),
            Some("E(Ljava/lang/Integer;)IS(I)I")
        );

        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_CAST_CHAIN", Some("0"))],
            || {
                let mut ctx = setup();
                let h = handle(&mut ctx, "p/C", "(I)I", MH_KIND_STATIC);
                let a1 = stamp(&mut ctx, h, "(Ljava/lang/Integer;)I", false);
                let e2 = stamp(&mut ctx, a1, "(Ljava/lang/Object;)I", true);
                assert!(mh_is_explicit_cast(&ctx, e2));
                assert_eq!(mh_cast_chain_text(&ctx, e2), None);
            },
        );
    }

    /// `findVirtual(Mid, m, (int)int).asType((Object,int)int).bindTo(v)`: the
    /// bound handle carries the chain minus the receiver, and a value the
    /// leaf receiver type refuses is recorded as the bound refusal that
    /// every invocation raises.
    #[test]
    fn bind_to_after_as_type_records_the_bound_values_refused_cast() {
        let mut ctx = setup();
        let h = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
        let wide = stamp(&mut ctx, h, "(Ljava/lang/Object;I)I", false);
        assert_eq!(
            mh_cast_chain_text(&ctx, wide).as_deref(),
            Some("S(Lp/Mid;I)I")
        );
        let arr = int_array(&mut ctx);
        let chain = bound_cast_chain(&mut ctx, wide, arr, MH_KIND_VIRTUAL);
        let expected = CastChain {
            bound_refusal: Some("Lp/Mid;".to_string()),
            links: vec![CastLink {
                explicit: false,
                desc: "(I)I".to_string(),
            }],
        };
        assert_eq!(chain.as_ref(), Some(&expected));
        let s = object(&mut ctx, "java/lang/String", Value::Int(0));
        let passes = bound_cast_chain(&mut ctx, wide, s, MH_KIND_VIRTUAL);
        assert_eq!(passes.and_then(|c| c.bound_refusal), None);
        // An unstamped handle has nothing to carry.
        assert_eq!(bound_cast_chain(&mut ctx, h, arr, MH_KIND_VIRTUAL), None);

        // The dispatch judge raises the recorded refusal against MH_BOUND.
        let bound = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
        ctx.set_field(bound, MH_BOUND, arr);
        let three = [Value::Int(3)];
        let r = mh_cast_chain_judge_args(&mut ctx, bound, &expected, Some("(I)I"), &three);
        assert_eq!(
            r.err().as_ref().and_then(cce_target).as_deref(),
            Some("p.Mid")
        );
    }

    /// The door's leaf pass no longer casts to an interface parameter of an
    /// `explicitCastArguments` copy; an `asType` copy still does.
    #[test]
    fn an_explicit_copy_passes_an_interface_parameter_uncast() {
        let mut ctx = setup();
        let h = handle(&mut ctx, "p/C", "(Lp/Greeter;)I", MH_KIND_STATIC);
        let explicit = stamp(&mut ctx, h, "(Ljava/lang/Object;)I", true);
        let strict = stamp(&mut ctx, h, "(Ljava/lang/Object;)I", false);
        let arr = int_array(&mut ctx);
        let k = MH_KIND_STATIC;
        let args = [arr];
        let r = invoke_reference_cast_refusal(&mut ctx, explicit, k, false, false, &args, None);
        assert!(r.is_none());
        let r = invoke_reference_cast_refusal(&mut ctx, strict, k, false, false, &args, None);
        assert!(r.is_some());
        // `invokeExact`'s inner cast, the same rule.
        let outer = Some("(Ljava/lang/Object;)I");
        assert!(invoke_exact_inner_cast_refusal(&mut ctx, explicit, k, outer, &[arr]).is_none());
        assert!(invoke_exact_inner_cast_refusal(&mut ctx, strict, k, outer, &[arr]).is_some());
        // And the entry-door cast `mh_entry_adapt` makes beneath the stamp.
        assert!(entry_cast_check(&mut ctx, arr, "Lp/Greeter;").is_err());
        MH_EXPLICIT_SCOPE.with(|f| f.set(true));
        let under_scope = entry_cast_check(&mut ctx, arr, "Lp/Greeter;").is_ok();
        let class_still_cast = entry_cast_check(&mut ctx, arr, "Lp/Sub;").is_err();
        MH_EXPLICIT_SCOPE.with(|f| f.set(false));
        assert!(under_scope && class_still_cast);
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_EXPLICIT_INTERFACE_UNCAST", Some("0"))],
            || {
                let r =
                    invoke_reference_cast_refusal(&mut ctx, explicit, k, false, false, &args, None);
                assert!(r.is_some());
            },
        );
    }

    /// `mh_type_returns_reference` answers what `mh_type_descriptor`'s
    /// return would, including its `MH_DESC` fallback.
    #[test]
    fn the_cheap_return_check_matches_the_full_descriptor() {
        let mut ctx = setup();
        let h = handle(&mut ctx, "p/C", "(I)I", MH_KIND_STATIC);
        assert!(!mh_type_returns_reference(&mut ctx, h));
        let boxed = stamp(&mut ctx, h, "(I)Ljava/lang/Object;", false);
        assert!(mh_type_returns_reference(&mut ctx, boxed));
        let untyped = handle(&mut ctx, "p/C", "(I)Ljava/lang/String;", MH_KIND_STATIC);
        ctx.set_field_by_name(untyped, "type", Value::Object(None));
        assert!(mh_type_returns_reference(&mut ctx, untyped));
        for mh in [h, boxed, untyped] {
            let full = mh_type_descriptor(&mut ctx, mh).unwrap_or_default();
            assert_eq!(
                mh_type_returns_reference(&mut ctx, mh),
                matches!(
                    return_type_desc(&full).as_bytes().first(),
                    Some(b'L' | b'[')
                )
            );
        }
    }

    /// The exact-class fast path of `reference_arg_admitted`.
    #[test]
    fn an_exact_class_is_admitted_without_a_lookup() {
        let mut ctx = setup();
        let s = object(&mut ctx, "java/lang/String", Value::Int(0));
        assert_eq!(
            reference_arg_admitted(&mut ctx, s, "Ljava/lang/String;"),
            Some(true)
        );
        let arr = int_array(&mut ctx);
        assert_eq!(
            reference_arg_admitted(&mut ctx, arr, "Lp/Sub;"),
            Some(false)
        );
    }

    /// Round 13 wave 1 (lane mhffm), residual 1 (`R12Mh5ChainResidual` R1):
    /// `retLong(int)long.explicitCastArguments((int)int).asType((int)long)`
    /// answers `(long)(int) r`; with `CRATONVM_MH_CAST_CHAIN_CONVERTS=0` the
    /// untruncated value, as in round 12.
    #[test]
    fn a_middle_link_narrowing_the_return_is_delivered() {
        let mut ctx = setup();
        let chain = CastChain {
            bound_refusal: None,
            links: vec![
                CastLink {
                    explicit: false,
                    desc: "(I)I".to_string(),
                },
                CastLink {
                    explicit: true,
                    desc: "(I)J".to_string(),
                },
            ],
        };
        let r = (5_i64 << 32) | 7;
        let got =
            mh_cast_chain_judge_return(&mut ctx, &chain, Some("(I)J"), Ok(Some(Value::Long(r))));
        assert_eq!(got.ok(), Some(Some(Value::Long(7))));
    }

    /// Round 13 wave 3 (lane ffm2), the cross-variant return:
    /// `retLong.explicitCastArguments((int)int).asType((int)double)` answers
    /// `(double)(int) r`. It is delivered in the LEAF's variant (`Long(7)`),
    /// which the doors widen to `7.0`; before, the untruncated `Long(r)` went
    /// on and was widened to `(double) r`.
    #[test]
    fn a_middle_link_narrowing_is_delivered_across_value_variants() {
        let mut ctx = setup();
        let chain = CastChain {
            bound_refusal: None,
            links: vec![
                CastLink {
                    explicit: false,
                    desc: "(I)I".to_string(),
                },
                CastLink {
                    explicit: true,
                    desc: "(I)J".to_string(),
                },
            ],
        };
        let r = (5_i64 << 32) | -3_i64 as u32 as i64;
        let got =
            mh_cast_chain_judge_return(&mut ctx, &chain, Some("(I)D"), Ok(Some(Value::Long(r))));
        assert_eq!(got.ok(), Some(Some(Value::Long(-3))));
        assert_eq!(
            widen_return_value(Value::Long(-3), b'J', b'D'),
            Some(Value::Double(-3.0))
        );
    }

    /// A chain answer no leaf-typed value maps to keeps the old answer:
    /// `(double)(float) Integer.MAX_VALUE` is `2^31`, which no `int` widens to.
    #[test]
    fn a_cross_variant_return_with_no_leaf_spelling_is_left_alone() {
        let mut ctx = setup();
        let chain = CastChain {
            bound_refusal: None,
            links: vec![
                CastLink {
                    explicit: false,
                    desc: "(I)F".to_string(),
                },
                CastLink {
                    explicit: false,
                    desc: "(I)I".to_string(),
                },
            ],
        };
        let got = mh_cast_chain_judge_return(
            &mut ctx,
            &chain,
            Some("(I)D"),
            Ok(Some(Value::Int(i32::MAX))),
        );
        assert_eq!(got.ok(), Some(Some(Value::Int(i32::MAX))));
        assert_eq!(
            chain_return_in_leaf_variant(Value::Double(2147483648.0), "I", "D"),
            None
        );
        assert_eq!(
            chain_return_in_leaf_variant(Value::Double(-3.0), "I", "D"),
            Some(Value::Int(-3))
        );
        // A sub-int leaf never gets a value outside its range.
        assert_eq!(
            chain_return_in_leaf_variant(Value::Long(300), "B", "J"),
            None
        );
    }

    /// The `invoke` door boxes against the declared return only for a
    /// reference call site over a primitive declared return the leaf does not
    /// share.
    #[test]
    fn invoke_boxes_the_declared_return_only_for_that_shape() {
        let obj = Some("()Ljava/lang/Object;");
        assert!(invoke_boxes_declared_return("()J", Some("()D"), obj));
        assert!(!invoke_boxes_declared_return("()J", Some("()J"), obj));
        assert!(!invoke_boxes_declared_return(
            "()J",
            Some("()D"),
            Some("()D")
        ));
        assert!(!invoke_boxes_declared_return(
            "()Ljava/lang/Long;",
            Some("()D"),
            obj
        ));
        assert!(!invoke_boxes_declared_return("()J", None, obj));
        assert!(!invoke_boxes_declared_return("()J", Some("()D"), None));
        // `invokeWithArguments`: its call site is always `Object`.
        assert!(invoke_with_arguments_boxes_declared_return(
            "(I)J",
            Some("(I)D")
        ));
        assert!(!invoke_with_arguments_boxes_declared_return(
            "(I)J",
            Some("(I)J")
        ));
        assert!(!invoke_with_arguments_boxes_declared_return(
            "(I)J",
            Some("(I)Ljava/lang/Object;")
        ));
        assert!(!invoke_with_arguments_boxes_declared_return("(I)J", None));
    }

    /// Residual 2 (`R12Mh5ChainResidual` R2):
    /// `idObj(Object)Object.asType((int)Object).asType((Object)Object)` hands
    /// `idObj` an `Integer` made from the caller's `Short`, not the `Short`.
    #[test]
    fn a_middle_primitive_link_reboxes_the_argument() {
        let mut ctx = setup();
        // Round 13 wave 3 (lane ffm2): a PRIVATE VM identity. The re-box goes
        // through `Integer.valueOf`, whose cache (`lang_math`
        // `integer_cache_table`) and wrapper class-id cache (`alloc_wrapper`)
        // are process-wide tables keyed by `vm_identity()`, which every mock
        // reports as `0` by default. Under the shared `0`, `Integer.valueOf(12)`
        // could hand back a box another test's mock allocated (or a class id
        // only that mock knows), whose class this mock cannot name: the
        // `left: None` failure. The production path is right; one VM, one
        // identity.
        ctx.set_vm_identity(0x13F3_0001);
        let h = handle(
            &mut ctx,
            "p/C",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            MH_KIND_STATIC,
        );
        let chain = CastChain {
            bound_refusal: None,
            links: vec![
                CastLink {
                    explicit: false,
                    desc: "(I)Ljava/lang/Object;".to_string(),
                },
                CastLink {
                    explicit: false,
                    desc: "(Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
                },
            ],
        };
        let outer = Some("(Ljava/lang/Object;)Ljava/lang/Object;");
        let short = object(&mut ctx, "java/lang/Short", Value::Int(12));
        let judged = mh_cast_chain_judge_args_rebox(&mut ctx, h, &chain, outer, &[short]);
        let (values, reboxes) = match judged {
            Ok(pair) => pair,
            Err(e) => panic!("the chain refused a Short: {e:?}"),
        };
        assert!(values.is_none());
        assert_eq!(reboxes.len(), 1);
        assert_eq!(reboxes[0].0, 0);
        assert_eq!(reboxes[0].1, DESC_INT);
        assert_eq!(reboxes[0].2, Value::Int(12));
        let (_h, out) = mh_cast_chain_rebox_args(&mut ctx, h, vec![short], &reboxes);
        let boxed = match out.first() {
            Some(Value::Object(Some(o))) => *o,
            other => panic!("no box: {other:?}"),
        };
        assert_ne!(Value::Object(Some(boxed)), short);
        assert_eq!(
            ctx.class_name_of_id(ctx.class_id_of_object(boxed))
                .as_deref(),
            Some("java/lang/Integer")
        );
    }
}

/// Interpreter round i1 wave 28, lane L4: an invoker handle checks its call
/// site and its target's type as HotSpot's `Invokers` do, and an
/// `explicitCastArguments` stamp passes a reference to an interface parameter
/// uncast through both doors. Probe:
/// `tools/probes/interp/L4/L4W28InvokerChecks.java`.
#[cfg(test)]
mod w28_invoker_check_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeHeapAccess};

    fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
        match err {
            MethodCallFailed::ExceptionThrown(o) => {
                ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                    == Some("java/lang/invoke/WrongMethodTypeException")
            }
            _ => false,
        }
    }

    fn method_type(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
        match build_method_type_from_descriptor(ctx, desc) {
            Ok(Some(mt)) => mt,
            other => panic!("MethodType {desc}: {other:?}"),
        }
    }

    /// `exactInvoker(t)`: marked exact, `MH_DESC` = `t`, and an `invokeExact`
    /// call site on it is judged against its own `(MethodHandle, t...)R`.
    /// Without `MH_DESC` (the old shape, whose `type` is `t`) it is not.
    #[test]
    fn an_exact_invoker_judges_its_call_site_against_its_own_type() {
        let mut ctx = MockNativeContext::new();
        let t = "(Ljava/lang/Object;)Ljava/lang/Object;";
        let ex = alloc_method_handle(&mut ctx, "", "", t, MH_KIND_INVOKER).unwrap();
        let own = method_type(
            &mut ctx,
            "(Ljava/lang/invoke/MethodHandle;Ljava/lang/Object;)Ljava/lang/Object;",
        );
        ctx.set_field_by_name(ex, "type", Value::Object(Some(own)));
        assert!(!mh_is_exact_invoker(&ctx, ex));
        mh_mark_exact_invoker(&ctx, ex);
        assert!(mh_is_exact_invoker(&ctx, ex));
        assert!(!mh_is_varargs_collector(&ctx, ex), "the bits are separate");
        assert!(invoker_has_own_type(&ctx, ex, MH_KIND_INVOKER));
        assert!(!invoker_has_own_type(&ctx, ex, MH_KIND_STATIC));

        let declared = mh_declared_descriptor(&mut ctx, ex).expect("the invoker's own type");
        assert!(
            exact_call_site_refusal(&mut ctx, ex, MH_KIND_INVOKER, &declared, Some(&declared))
                .is_none()
        );
        let short_site = "(Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object;";
        let refusal =
            exact_call_site_refusal(&mut ctx, ex, MH_KIND_INVOKER, short_site, Some(&declared))
                .expect("a site one argument short is refused");
        assert!(is_wmte(&ctx, &refusal));

        ctx.set_field(ex, MH_DESC, Value::Object(None));
        assert!(!invoker_has_own_type(&ctx, ex, MH_KIND_INVOKER));
        assert!(exact_call_site_refusal(
            &mut ctx,
            ex,
            MH_KIND_INVOKER,
            short_site,
            Some(&declared)
        )
        .is_none());
    }

    /// A null target is HotSpot's helpful NPE, naming the call the invoker's
    /// `LambdaForm` makes on it (measured on 25.0.3).
    #[test]
    fn a_null_target_names_the_call_the_invoker_makes_on_it() {
        let message = |e: MethodCallFailed| match e {
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { message, .. },
            )) => message,
            other => panic!("not an NPE: {other:?}"),
        };
        assert_eq!(
            message(invoker_null_target(true)).as_deref(),
            Some("Cannot invoke \"java.lang.invoke.MethodHandle.type()\" because \"mh\" is null")
        );
        assert_eq!(
            message(invoker_null_target(false)).as_deref(),
            Some(
                "Cannot invoke \"java.lang.invoke.MethodHandle.asType(java.lang.invoke.MethodType)\" \
                 because \"mh\" is null"
            )
        );
    }

    /// The door's boxed answer goes back raw for a primitive target return, so
    /// the invoker's own door can still widen it against its call site.
    #[test]
    fn a_primitive_target_return_goes_back_raw() {
        let mut ctx = MockNativeContext::new();
        let boxed = match ctx.new_object("java/lang/Integer").unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("Integer: {other:?}"),
        };
        ctx.set_field(boxed, 0, Value::Int(3));
        let b = Some(Value::Object(Some(boxed)));
        assert_eq!(invoker_result_unboxed(&ctx, b, "I"), Some(Value::Int(3)));
        assert_eq!(invoker_result_unboxed(&ctx, b, "J"), Some(Value::Long(3)));
        assert_eq!(invoker_result_unboxed(&ctx, b, "Ljava/lang/Object;"), b);
        assert_eq!(
            invoker_result_unboxed(&ctx, Some(Value::Int(4)), "J"),
            Some(Value::Long(4))
        );
        // `void` -> primitive is the zero, as `asType` makes it.
        assert_eq!(invoker_result_unboxed(&ctx, None, "I"), Some(Value::Int(0)));
        assert_eq!(
            invoker_result_unboxed(&ctx, Some(Value::Object(None)), "D"),
            Some(Value::Double(0.0))
        );
        assert_eq!(invoker_result_unboxed(&ctx, None, "V"), None);
    }

    /// Stage 1 of the unmount proposal: the door's `MH_TAIL` token, and which
    /// leaf returns run transparent under it with which conversion owed.
    #[test]
    fn a_tail_token_admits_exactly_the_conversions_a_remount_can_make() {
        assert_eq!(door_tail_token("Ljava/lang/Object;"), b'L');
        assert_eq!(door_tail_token("[I"), b'L');
        assert_eq!(door_tail_token("I"), b'I');
        assert_eq!(door_tail_token("V"), b'V');
        assert_eq!(door_tail_token(""), 0);

        // Stage 0 and the identities: nothing owed.
        assert_eq!(tail_leaf_conversion(b'L', b'L'), Some(None));
        assert_eq!(tail_leaf_conversion(b'L', b'['), Some(None));
        assert_eq!(tail_leaf_conversion(b'I', b'I'), Some(None));
        assert_eq!(tail_leaf_conversion(b'V', b'V'), Some(None));
        // A primitive boxed or widened: the remounted frame converts.
        assert_eq!(tail_leaf_conversion(b'L', b'I'), Some(Some(DESC_OBJECT)));
        assert_eq!(tail_leaf_conversion(b'L', b'Z'), Some(Some(DESC_OBJECT)));
        assert_eq!(tail_leaf_conversion(b'J', b'I'), Some(Some(DESC_LONG)));
        assert_eq!(tail_leaf_conversion(b'D', b'F'), Some(Some(DESC_DOUBLE)));
        // Opaque: no tail, a void behind a value, a reference behind a
        // primitive, a narrowing, a boolean widened.
        assert_eq!(tail_leaf_conversion(0, b'L'), None);
        assert_eq!(tail_leaf_conversion(b'L', b'V'), None);
        assert_eq!(tail_leaf_conversion(b'I', b'L'), None);
        assert_eq!(tail_leaf_conversion(b'I', b'J'), None);
        assert_eq!(tail_leaf_conversion(b'I', b'Z'), None);
        assert_eq!(tail_leaf_conversion(b'V', b'I'), None);
    }

    /// Interpreter round i6 wave 115, lane L4 (`CRATONVM_MH_VOID_TAIL_UNMOUNT`):
    /// with the admission, a `void` leaf behind a reference token owes the
    /// door's `null` (an `Object` caller return); nothing else moves, and a
    /// `void` leaf behind a primitive token stays opaque.
    #[test]
    fn a_void_leaf_behind_a_reference_owes_null_only_when_admitted() {
        assert_eq!(tail_leaf_conversion_with(b'L', b'V', false), None);
        assert_eq!(
            tail_leaf_conversion_with(b'L', b'V', true),
            Some(Some(DESC_OBJECT))
        );
        assert_eq!(tail_leaf_conversion_with(b'I', b'V', true), None);
        assert_eq!(tail_leaf_conversion_with(b'V', b'V', true), Some(None));
        assert_eq!(tail_leaf_conversion_with(b'L', b'I', true), Some(Some(DESC_OBJECT)));
        assert_eq!(tail_leaf_conversion_with(b'L', b'L', true), Some(None));
        assert_eq!(tail_leaf_conversion_with(0, b'V', true), None);
    }

    /// A context that keeps no frames (the mock's `continuation_frame_mark`
    /// is the trait default) keeps a door opaque when a conversion is owed,
    /// and transparent when none is.
    #[test]
    fn a_leaf_that_owes_a_conversion_needs_a_frame_mark() {
        let ctx = MockNativeContext::new();
        assert!(transparent_leaf_call(&ctx, b'L', "()I", None).is_none());
        let identity = transparent_leaf_call(&ctx, b'I', "()I", None);
        assert!(identity.as_ref().is_some_and(|c| c.owed.is_none()));
        assert_eq!(
            cratonvm_native_api::continuation_pin::transparent_natives(),
            1
        );
        drop(identity);
        assert_eq!(
            cratonvm_native_api::continuation_pin::transparent_natives(),
            0
        );
        assert!(transparent_leaf_call(&ctx, 0, "()Ljava/lang/String;", None).is_none());
    }

    /// `explicitCastArguments(takesRunner, (Object)String)`: the step to the
    /// leaf's INTERFACE parameter is JLS 5.5's, which does not cast, in the
    /// `invokeExact` door (round 12's inner cast) and the `invoke` /
    /// `invokeWithArguments` doors' leaf pass alike; an `asType` copy casts.
    /// The mock gives a positive mismatch only for an array, so the argument
    /// is one.
    #[test]
    fn an_explicit_cast_passes_a_reference_to_an_interface_parameter_uncast() {
        let mut ctx = MockNativeContext::new();
        let runner = ctx
            .ensure_class_initialized("p/Runner")
            .unwrap_or(ClassId::new(0));
        ctx.set_is_interface(runner, true);
        let mh = alloc_method_handle(
            &mut ctx,
            "p/C",
            "m",
            "(Lp/Runner;)Ljava/lang/String;",
            MH_KIND_STATIC,
        )
        .unwrap();
        let widened_desc = "(Ljava/lang/Object;)Ljava/lang/String;";
        let widened = method_type(&mut ctx, widened_desc);
        ctx.set_field_by_name(mh, "type", Value::Object(Some(widened)));
        let arr = ctx.new_array(ArrayElementType::Int, 1);
        let args = [Value::Object(Some(arr))];

        let as_type_exact = invoke_exact_inner_cast_refusal(
            &mut ctx,
            mh,
            MH_KIND_STATIC,
            Some(widened_desc),
            &args,
        );
        assert!(
            as_type_exact.is_some(),
            "an asType copy casts to the interface"
        );
        let as_type_invoke =
            invoke_reference_cast_refusal(&mut ctx, mh, MH_KIND_STATIC, false, false, &args, None);
        assert!(
            as_type_invoke.is_some(),
            "an asType copy casts to the interface"
        );

        mh_mark_explicit_cast(&ctx, mh);
        assert!(invoke_exact_inner_cast_refusal(
            &mut ctx,
            mh,
            MH_KIND_STATIC,
            Some(widened_desc),
            &args
        )
        .is_none());
        assert!(invoke_reference_cast_refusal(
            &mut ctx,
            mh,
            MH_KIND_STATIC,
            false,
            false,
            &args,
            None
        )
        .is_none());
    }
}

/// Round 15 wave 1, lane mh: the direct lane's per-thread record memo
/// (`DIRECT_LANE_MEMO`, proposal C14-1), the decoded `invoke` refusal and the
/// lean virtual arm. Mock conventions as `r13w10_callcost4_direct_lane_tests`
/// below (a positive reference mismatch is an array).
#[cfg(test)]
mod r15w1_mh_direct_lane_record_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// A mock with a PRIVATE VM identity (`0` is never memoised) and
    /// `MethodType`'s `rtype` / `ptypes` declared.
    fn setup(private_vm: bool) -> MockNativeContext {
        static NEXT: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0x1531_0000);
        let mut ctx = MockNativeContext::new();
        if private_vm {
            ctx.set_vm_identity(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        }
        for name in ["p/C", "p/Mid", "p/Sub"] {
            let _ = ctx.ensure_class_initialized(name);
        }
        let probe = mt(&mut ctx, "()V");
        let cid = ctx.class_id_of_object(probe);
        let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            cid,
            vec![
                field("rtype", "Ljava/lang/Class;", 0),
                field("ptypes", "[Ljava/lang/Class;", 1),
            ],
        );
        ctx
    }

    fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
        match build_method_type_from_descriptor(ctx, desc) {
            Ok(Some(mt)) => mt,
            other => panic!("MethodType {desc}: {other:?}"),
        }
    }

    fn handle(
        ctx: &mut MockNativeContext,
        class: &str,
        name: &str,
        desc: &str,
        kind: i32,
    ) -> ObjectRef {
        match alloc_method_handle(ctx, class, name, desc, kind) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc {class}.{name}{desc}: {e:?}"),
        }
    }

    fn object(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
        let o = match ctx.new_object(class) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("new_object {class}: {other:?}"),
        };
        ctx.set_field(o, 0, payload);
        Value::Object(Some(o))
    }

    /// The own types of this thread's memo lines filed for `vm`.
    fn memoised(vm: usize) -> Vec<String> {
        DIRECT_LANE_MEMO.with(|memo| {
            memo.borrow()
                .iter()
                .filter(|line| line.vm == vm)
                .filter_map(|line| line.record.as_ref().map(|r| r.own_type.clone()))
                .collect()
        })
    }

    /// A memoised record is re-validated against the handle's field objects on
    /// every call: a `type` restamped in place and an `MH_DESC` rewritten in
    /// place are both seen at once (a record trusted by the handle's identity
    /// would keep answering at `(I)I`).
    #[test]
    fn a_memoised_record_sees_every_in_place_rewrite_of_its_fields() {
        let mut ctx = setup(true);
        let exact = DirectLaneDoor::InvokeExact;
        let mh = handle(&mut ctx, "p/C", "m", "(I)I", MH_KIND_STATIC);
        for _ in 0..2 {
            let r = mh_direct_lane(&mut ctx, mh, &[Value::Int(1)], Some("(I)I"), exact);
            assert!(r.is_some(), "served at its own type");
        }
        if mh_direct_lane_record_enabled() {
            assert!(
                memoised(ctx.vm_identity()).iter().any(|t| t == "(I)I"),
                "the record is memoised"
            );
        }
        // `type` restamped IN PLACE: the declared type is no longer the own one.
        let long_type = mt(&mut ctx, "(J)I");
        ctx.set_field_by_name(mh, "type", Value::Object(Some(long_type)));
        assert!(mh_direct_lane(&mut ctx, mh, &[Value::Int(1)], Some("(I)I"), exact).is_none());
        // `MH_DESC` rewritten IN PLACE to agree with it: served at the new type
        // only.
        let long_desc = ctx.create_string("(J)I");
        ctx.set_field(mh, MH_DESC, Value::Object(Some(long_desc)));
        assert!(mh_direct_lane(&mut ctx, mh, &[Value::Long(1)], Some("(J)I"), exact).is_some());
        assert!(mh_direct_lane(&mut ctx, mh, &[Value::Int(1)], Some("(I)I"), exact).is_none());
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    /// `asType` over a handle, as the `asType` native runs it.
    fn stamp(ctx: &mut MockNativeContext, h: ObjectRef, desc: &str) -> ObjectRef {
        let t = mt(ctx, desc);
        match mh_with_stamped_type(ctx, Value::Object(Some(h)), t, false) {
            Ok(Some(Value::Object(Some(copy)))) => copy,
            other => panic!("stamp {desc}: {other:?}"),
        }
    }

    /// `r15w1-mh-exact-invoker-and-cast-chain-share-one-marking-bit`: an
    /// `asType` copy of an `exactInvoker` stays exact (the stamp clears the
    /// cast-chain bit, which was the exact-invoker bit), and an active cast
    /// chain does not read as an exact invoker.
    #[test]
    fn the_exact_invoker_and_cast_chain_markings_are_separate_bits() {
        let mut ctx = setup(true);
        assert_eq!(MH_MARK_CAST_CHAIN & MH_MARK_EXACT_INVOKER, 0);
        let ex = handle(&mut ctx, "", "", "(I)I", MH_KIND_INVOKER);
        let own = mt(&mut ctx, "(Ljava/lang/invoke/MethodHandle;I)I");
        ctx.set_field_by_name(ex, "type", Value::Object(Some(own)));
        mh_mark_exact_invoker(&ctx, ex);
        let copy = stamp(
            &mut ctx,
            ex,
            "(Ljava/lang/invoke/MethodHandle;Ljava/lang/Integer;)I",
        );
        assert_ne!(Value::Object(Some(copy)), Value::Object(Some(ex)), "a copy");
        assert!(mh_is_exact_invoker(&ctx, ex), "the receiver is untouched");
        assert!(mh_is_exact_invoker(&ctx, copy), "the copy is still exact");
        // Two stamps over a direct handle: an active chain.
        let h = handle(
            &mut ctx,
            "p/C",
            "m",
            "(Ljava/lang/Object;)I",
            MH_KIND_STATIC,
        );
        let c1 = stamp(&mut ctx, h, "(Lp/Sub;)I");
        let c2 = stamp(&mut ctx, c1, "(Ljava/lang/Object;)I");
        assert_ne!(
            mh_marking_bits(&ctx, c2) & MH_MARK_CAST_CHAIN,
            0,
            "an active chain"
        );
        assert!(
            !mh_is_exact_invoker(&ctx, c2),
            "a chain is not an exact invoker"
        );
    }

    /// A context whose VM identity is `0` (every mock by default, and any
    /// wrapper that does not forward it) is served without being memoised.
    #[test]
    fn a_context_without_a_vm_identity_is_never_memoised() {
        let mut ctx = setup(false);
        assert_eq!(ctx.vm_identity(), 0);
        let mh = handle(&mut ctx, "p/C", "m", "(I)I", MH_KIND_STATIC);
        let r = mh_direct_lane(
            &mut ctx,
            mh,
            &[Value::Int(1)],
            Some("(I)I"),
            DirectLaneDoor::InvokeExact,
        );
        assert!(r.is_some());
        assert!(memoised(0).is_empty(), "{:?}", memoised(0));
    }

    /// The `invoke` door's reference refusal asked with the record's decoded
    /// parameters and class answers exactly what the decoding one answers.
    /// No declared type is handed in, so the leaf's own receiver cast (the
    /// class the record carries) and parameter casts are the ones judged.
    #[test]
    fn the_decoded_invoke_refusal_answers_as_the_decoding_one() {
        let mut ctx = setup(true);
        let mh = handle(&mut ctx, "p/Mid", "m", "(Lp/Sub;)I", MH_KIND_VIRTUAL);
        let recv = object(&mut ctx, "p/Mid", Value::Int(0));
        let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
        let site = Some("(Ljava/lang/Object;Ljava/lang/Object;)I");
        let params = ["Lp/Sub;".to_string()];
        for (row, args) in [
            ("clean", [recv, Value::Object(None)]),
            ("array argument", [recv, arr]),
            ("array receiver", [arr, Value::Object(None)]),
        ] {
            let with = invoke_reference_cast_refusal_with(
                &mut ctx,
                mh,
                MH_KIND_VIRTUAL,
                true,
                false,
                &args,
                site,
                None,
            );
            let decoded = invoke_reference_cast_refusal_decoded(
                &mut ctx,
                mh,
                MH_KIND_VIRTUAL,
                true,
                false,
                &args,
                site,
                None,
                &params,
                Some("p/Mid"),
            );
            assert_eq!(format!("{with:?}"), format!("{decoded:?}"), "{row}");
            assert_eq!(with.is_none(), row == "clean", "{row}: {with:?}");
        }
    }

    /// `readResolve()Object` missing on the leaf: the neutral answer is the
    /// receiver. The hook plays a moving collection inside the call: it
    /// re-addresses every pin of the receiver to the object the receiver's
    /// field 0 names.
    fn missing_read_resolve(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        name: &str,
        desc: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        if name != "readResolve" {
            return None;
        }
        if let Value::Object(Some(moved)) = ctx.get_field(receiver, 0) {
            ctx.remap_native_pin_addr_for_test(receiver.as_ptr() as usize, moved.as_ptr() as usize);
        }
        Some(Err(MethodCallFailed::InternalError(VmError::Linkage(
            LinkageError::NoSuchMethodError {
                class_name: "p/Mid".to_string(),
                method_name: name.to_string(),
                method_descriptor: desc.to_string(),
            },
        ))))
    }

    /// The lean virtual arm answers the receiver where the call left it; the
    /// round-13 arm answered its pre-call copy.
    #[test]
    fn the_lean_virtual_arm_answers_the_receiver_where_the_call_left_it() {
        let mut ctx = setup(true);
        let moved = object(&mut ctx, "p/Mid", Value::Int(0));
        let recv = object(&mut ctx, "p/Mid", moved);
        ctx.set_invoke_virtual_hook(missing_read_resolve);
        let mh = handle(
            &mut ctx,
            "p/Mid",
            "readResolve",
            "()Ljava/lang/Object;",
            MH_KIND_VIRTUAL,
        );
        let r = mh_direct_lane(
            &mut ctx,
            mh,
            &[recv],
            Some("(Lp/Mid;)Ljava/lang/Object;"),
            DirectLaneDoor::InvokeExact,
        );
        let answered = match r {
            Some(Ok(Some(v))) => v,
            other => panic!("the neutral answer: {other:?}"),
        };
        if mh_direct_lane_lean_virtual_enabled() {
            assert_eq!(answered, moved, "the receiver's current address");
        }
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    /// `r15w1-mh-neutral-serialization-hook-answers-a-stale-receiver`: the
    /// ordinary dispatch body's `VIRTUAL` arms (unbound and bound, reached
    /// through `mh_dispatch`, not the lane) answer the receiver where the call
    /// left it too.
    #[test]
    fn the_dispatch_body_answers_the_receiver_where_the_call_left_it() {
        let mut ctx = setup(true);
        ctx.set_invoke_virtual_hook(missing_read_resolve);
        let neutral =
            |ctx: &mut MockNativeContext, mh: ObjectRef, extra: &[Value]| match mh_dispatch(
                ctx, mh, extra,
            ) {
                Ok(Some(v)) => v,
                other => panic!("the neutral answer: {other:?}"),
            };
        // Unbound.
        let moved = object(&mut ctx, "p/Mid", Value::Int(0));
        let recv = object(&mut ctx, "p/Mid", moved);
        let mh = handle(
            &mut ctx,
            "p/Mid",
            "readResolve",
            "()Ljava/lang/Object;",
            MH_KIND_VIRTUAL,
        );
        let answered = neutral(&mut ctx, mh, &[recv]);
        if mh_neutral_hook_current_receiver_enabled() {
            assert_eq!(answered, moved, "unbound: the receiver's current address");
        }
        // Bound (`bindTo`'s shape: the receiver in `MH_BOUND`).
        let moved = object(&mut ctx, "p/Mid", Value::Int(0));
        let recv = object(&mut ctx, "p/Mid", moved);
        let bound = handle(
            &mut ctx,
            "p/Mid",
            "readResolve",
            "()Ljava/lang/Object;",
            MH_KIND_VIRTUAL,
        );
        ctx.set_field(bound, MH_BOUND, recv);
        let answered = neutral(&mut ctx, bound, &[]);
        if mh_neutral_hook_current_receiver_enabled() {
            assert_eq!(answered, moved, "bound: the receiver's current address");
        }
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }
}

/// Round 15 wave 4, lane mh2: MH15-1 (the direct lane's `invoke` refusal is
/// provably empty at the exact type, `own_type_aligned`). Mock conventions as
/// `r13w10_callcost4_direct_lane_tests` (a positive reference mismatch is an
/// array).
#[cfg(test)]
mod r15w4_mh2_direct_lane_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn setup() -> MockNativeContext {
        let mut ctx = MockNativeContext::new();
        for name in ["p/C", "p/Mid", "p/Sub"] {
            let _ = ctx.ensure_class_initialized(name);
        }
        let probe = mt(&mut ctx, "()V");
        let cid = ctx.class_id_of_object(probe);
        let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            cid,
            vec![
                field("rtype", "Ljava/lang/Class;", 0),
                field("ptypes", "[Ljava/lang/Class;", 1),
            ],
        );
        ctx
    }

    fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
        match build_method_type_from_descriptor(ctx, desc) {
            Ok(Some(mt)) => mt,
            other => panic!("MethodType {desc}: {other:?}"),
        }
    }

    fn handle(ctx: &mut MockNativeContext, class: &str, desc: &str, kind: i32) -> ObjectRef {
        match alloc_method_handle(ctx, class, "m", desc, kind) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc {class}: {e:?}"),
        }
    }

    fn object(ctx: &mut MockNativeContext, class: &str) -> Value {
        match ctx.new_object(class) {
            Ok(Some(v @ Value::Object(Some(_)))) => v,
            other => panic!("new_object {class}: {other:?}"),
        }
    }

    /// `m(Sub, int)int` answers its int plus 100.
    fn plus_hundred(
        _ctx: &mut MockNativeContext,
        _receiver: ObjectRef,
        name: &str,
        desc: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        match (name, desc, args) {
            ("m", "(Lp/Sub;I)I", [_, Value::Int(x)]) => Some(Ok(Some(Value::Int(x + 100)))),
            _ => None,
        }
    }

    fn cce_target(r: &Option<MethodCallResult>) -> Option<String> {
        match r {
            Some(Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { message, .. },
            )))) => message.rsplit(" to ").next().map(str::to_string),
            _ => None,
        }
    }

    /// The alignment bit: `true` for every shape the lane admits, `false` for
    /// a class name whose `;` misaligns the own type's split.
    #[test]
    fn the_own_type_is_aligned_for_every_real_class_name() {
        let mut ctx = setup();
        // String literals only (`r13_misc10_create_string_ratchet` counts the
        // computed-text `create_string` sites of this crate).
        let name = ctx.create_string("m");
        let rows = [
            (
                ctx.create_string("p/Mid"),
                ctx.create_string("(Lp/Sub;I)I"),
                true,
                true,
            ),
            (
                ctx.create_string("p/Mid"),
                ctx.create_string("()V"),
                true,
                true,
            ),
            (
                ctx.create_string("p/C"),
                ctx.create_string("(Lp/Sub;[IJ)I"),
                false,
                true,
            ),
            (
                ctx.create_string("p/C"),
                ctx.create_string("()I"),
                false,
                true,
            ),
            (
                ctx.create_string("p/a;b"),
                ctx.create_string("(I)I"),
                true,
                false,
            ),
        ];
        for (row, (class, desc, unbound, aligned)) in rows.into_iter().enumerate() {
            let fields = [
                Value::Object(Some(class)),
                Value::Object(Some(name)),
                Value::Object(Some(desc)),
                Value::Object(None),
            ];
            let record = mh_direct_lane_decode(&mut ctx, fields, unbound, None);
            assert_eq!(
                record.map(|r| r.own_type_aligned),
                Some(aligned),
                "row {row}"
            );
        }
    }

    /// MH15-1's claim, asked of the refusal itself: at site = declared = own
    /// type it answers `None` for every argument row -- clean, `null`, and the
    /// positive mismatches (an array receiver, an array argument) that the
    /// lane's LEAF casts refuse later.
    #[test]
    fn the_invoke_refusal_is_empty_at_the_exact_type() {
        if !mh_direct_lane_invoke_refusal_skip_enabled() {
            return;
        }
        let mut ctx = setup();
        let mid = object(&mut ctx, "p/Mid");
        let sub = object(&mut ctx, "p/Sub");
        let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
        let params = ["Lp/Sub;".to_string(), "I".to_string()];
        // Unbound virtual: the receiver leads.
        let virt = handle(&mut ctx, "p/Mid", "(Lp/Sub;I)I", MH_KIND_VIRTUAL);
        let own = "(Lp/Mid;Lp/Sub;I)I";
        for (row, args) in [
            ("clean", [mid, sub, Value::Int(1)]),
            ("null argument", [mid, Value::Object(None), Value::Int(1)]),
            ("array receiver", [arr, sub, Value::Int(1)]),
            ("array argument", [mid, arr, Value::Int(1)]),
        ] {
            for decoded in [true, false] {
                let r = if decoded {
                    invoke_reference_cast_refusal_decoded(
                        &mut ctx,
                        virt,
                        MH_KIND_VIRTUAL,
                        true,
                        false,
                        &args,
                        Some(own),
                        Some(own),
                        &params,
                        Some("p/Mid"),
                    )
                } else {
                    invoke_reference_cast_refusal_with(
                        &mut ctx,
                        virt,
                        MH_KIND_VIRTUAL,
                        true,
                        false,
                        &args,
                        Some(own),
                        Some(own),
                    )
                };
                assert!(r.is_none(), "virtual {row} (decoded {decoded}): {r:?}");
            }
        }
        // Static (and a bound virtual's shape: no receiver taken).
        let stat = handle(&mut ctx, "p/C", "(Lp/Sub;I)I", MH_KIND_STATIC);
        let own = "(Lp/Sub;I)I";
        for (row, args) in [
            ("clean", [sub, Value::Int(1)]),
            ("array argument", [arr, Value::Int(1)]),
        ] {
            for (mh, kind, bound) in [(stat, MH_KIND_STATIC, false), (virt, MH_KIND_VIRTUAL, true)]
            {
                let r = invoke_reference_cast_refusal_decoded(
                    &mut ctx,
                    mh,
                    kind,
                    kind == MH_KIND_VIRTUAL,
                    bound,
                    &args,
                    Some(own),
                    Some(own),
                    &params,
                    Some("p/Mid"),
                );
                assert!(r.is_none(), "kind {kind} {row}: {r:?}");
            }
        }
    }

    /// End to end: the lane's `invoke` answers what its `invokeExact` answers
    /// at the exact type (the exact door never asked the skipped refusal):
    /// the leaf's value, or the leaf cast's `ClassCastException`.
    #[test]
    fn the_lane_invoke_answers_as_invoke_exact_at_the_exact_type() {
        let mut ctx = setup();
        ctx.set_invoke_virtual_hook(plus_hundred);
        let mid = object(&mut ctx, "p/Mid");
        let sub = object(&mut ctx, "p/Sub");
        let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
        let virt = handle(&mut ctx, "p/Mid", "(Lp/Sub;I)I", MH_KIND_VIRTUAL);
        let site = Some("(Lp/Mid;Lp/Sub;I)I");
        for (row, args, cce) in [
            ("clean", [mid, sub, Value::Int(5)], None),
            ("array receiver", [arr, sub, Value::Int(5)], Some("p.Mid")),
            ("array argument", [mid, arr, Value::Int(5)], Some("p.Sub")),
        ] {
            let exact = mh_direct_lane(&mut ctx, virt, &args, site, DirectLaneDoor::InvokeExact);
            let invoke = mh_direct_lane(&mut ctx, virt, &args, site, DirectLaneDoor::Invoke);
            // A boxed answer (raw return off) is a fresh box per door.
            if cce.is_some() || mh_direct_lane_raw_return_enabled() {
                assert_eq!(format!("{exact:?}"), format!("{invoke:?}"), "{row}");
            }
            assert_eq!(cce_target(&invoke).as_deref(), cce, "{row}: {invoke:?}");
            if cce.is_none() {
                assert!(matches!(invoke, Some(Ok(Some(_)))), "{row}: {invoke:?}");
            }
        }
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    fn is_npe(r: &MethodCallResult) -> bool {
        matches!(
            r,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { .. }
            )))
        )
    }

    /// `r15w4-mh2-invoke-null-receiver-before-astype`: the `invoke` door's
    /// `null` receiver is refused AFTER the call site's
    /// `WrongMethodTypeException` and the argument casts, as HotSpot's
    /// `asType(callSiteType)` + adapter order has it; with neither, it is
    /// still the NPE.
    #[test]
    fn the_invoke_door_refuses_a_null_receiver_after_the_casts() {
        if !mh_strict_exact_signature() {
            return;
        }
        let mut ctx = setup();
        let after = mh_invoke_null_receiver_after_casts_enabled();
        let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
        let sub = object(&mut ctx, "p/Sub");
        let null = Value::Object(None);
        // A reference argument the adapter's cast refuses.
        let takes_sub = handle(&mut ctx, "p/Mid", "(Lp/Sub;)I", MH_KIND_VIRTUAL);
        let site = Some("(Lp/Mid;Ljava/lang/Object;)I".to_string());
        let r = mh_invoke_door(&mut ctx, takes_sub, &[null, arr], site.as_deref(), true);
        assert_eq!(is_npe(&r), !after, "cast before the receiver: {r:?}");
        if after {
            assert_eq!(cce_target(&Some(r)).as_deref(), Some("p.Sub"));
        }
        // A call site `asType` cannot convert to (`long` into `int`).
        let takes_int = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
        let narrow = Some("(Lp/Mid;J)I".to_string());
        let r = mh_invoke_door(
            &mut ctx,
            takes_int,
            &[null, Value::Long(5)],
            narrow.as_deref(),
            true,
        );
        assert!(r.is_err(), "{r:?}");
        assert_eq!(
            is_npe(&r),
            !after,
            "the site's type before the receiver: {r:?}"
        );
        // Nothing else wrong: the receiver's NPE, in both orders.
        let r = mh_invoke_door(&mut ctx, takes_sub, &[null, sub], site.as_deref(), true);
        assert!(is_npe(&r), "{r:?}");
    }
}

/// Round 15 wave 4, lane mh3: `bindTo` mints a FIXED-arity handle
/// (`r15w4-mh2-bindto-keeps-varargs-collector-FIXED-20260930.md`, HotSpot ref
/// `R15Mh2BoundVarargs`), the doors refuse its other arities, the ordinary mint
/// shares the source's strings (MH2-3), and the kinds whose dispatch does not
/// take `MH_BOUND` as the leading argument get the INSERT adapter. The handles
/// carry no `type` where a test does not need one (the mock's mirrors are not
/// asked), so each `type` question is answered from `MH_DESC`.
#[cfg(test)]
mod r15w4_mh3_bindto_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    const CAT: &str = "(Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/String;";
    const BOUND_CAT: &str = "([Ljava/lang/String;)Ljava/lang/String;";
    const SPREAD_SITE: &str = "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;";

    fn untyped(ctx: &mut MockNativeContext, class: &str, desc: &str, kind: i32) -> ObjectRef {
        let mh = match alloc_method_handle(ctx, class, "m", desc, kind) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc {class}: {e:?}"),
        };
        ctx.set_field_by_name(mh, "type", Value::Object(None));
        mh
    }

    fn bind_to(ctx: &mut MockNativeContext, target: ObjectRef, value: Value) -> ObjectRef {
        let mut registry = NativeMethodRegistry::new();
        register_t4_method_handle_invoke(&mut registry);
        let bind = registry
            .find(
                "java/lang/invoke/MethodHandle",
                "bindTo",
                "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
            )
            .expect("MethodHandle.bindTo should be registered");
        match bind(ctx, &[Value::Object(Some(target)), value]) {
            Ok(Some(Value::Object(Some(h)))) => h,
            other => panic!("bindTo answered {other:?}"),
        }
    }

    fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
        match err {
            MethodCallFailed::ExceptionThrown(o) => {
                ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                    == Some("java/lang/invoke/WrongMethodTypeException")
            }
            _ => false,
        }
    }

    /// The defect: a collector's bind is not a collector, the source keeps its
    /// marking, and the bound value is the one captured. With the shared mint
    /// the three strings are the source's own objects.
    #[test]
    fn a_bound_collector_is_fixed_arity_and_shares_the_strings() {
        let mut ctx = MockNativeContext::new();
        let cat = untyped(&mut ctx, "p/C", CAT, MH_KIND_STATIC);
        mh_set_varargs_collector(&ctx, cat, true);
        let dash = Value::Object(Some(ctx.create_string("-")));
        let bound = bind_to(&mut ctx, cat, dash);
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
        assert_eq!(
            mh_is_varargs_collector(&ctx, bound),
            !mh_bindto_fixed_arity_enabled()
        );
        assert!(
            mh_is_varargs_collector(&ctx, cat),
            "the source keeps its marking"
        );
        assert_eq!(ctx.get_field(bound, MH_KIND), Value::Int(MH_KIND_STATIC));
        assert_eq!(ctx.get_field(bound, MH_BOUND), dash);
        assert_eq!(mh_read_desc(&ctx, bound).as_deref(), Some(CAT));
        assert_eq!(mh_read_class(&ctx, bound).as_deref(), Some("p/C"));
        if bindto_shared_mint_applies(&ctx, cat, MH_KIND_STATIC) {
            for slot in [MH_CLASS, MH_NAME, MH_DESC] {
                assert_eq!(
                    ctx.get_field(bound, slot),
                    ctx.get_field(cat, slot),
                    "slot {slot}"
                );
            }
        }
    }

    /// The bound `type` the shared mint builds: the leading parameter dropped
    /// for a static-like kind, the receiver-less descriptor (or the stamped
    /// type minus its receiver) for a virtual one.
    #[test]
    fn the_bound_type_drops_what_the_bind_captured() {
        let mut ctx = MockNativeContext::new();
        let cat = untyped(&mut ctx, "p/C", CAT, MH_KIND_STATIC);
        assert_eq!(
            bindto_bound_type_descriptor(&mut ctx, cat, MH_KIND_STATIC, CAT, None),
            BOUND_CAT
        );
        let fmt = "([Ljava/lang/Object;)Ljava/lang/String;";
        let virt = untyped(&mut ctx, "java/lang/String", fmt, MH_KIND_VIRTUAL);
        assert_eq!(
            bindto_bound_type_descriptor(&mut ctx, virt, MH_KIND_VIRTUAL, fmt, None),
            fmt
        );
        assert_eq!(
            bindto_bound_type_descriptor(&mut ctx, virt, MH_KIND_VIRTUAL, fmt, Some("(I)I")),
            "(I)I"
        );
    }

    /// `asType`'s arity refusal: only a bound, non-collector direct handle at
    /// a site of another arity, and only when its `type` is the leaf's minus
    /// the capture.
    #[test]
    fn a_bound_handle_refuses_another_arity() {
        if !mh_bindto_fixed_arity_enabled() || !mh_strict_exact_signature() {
            return;
        }
        let mut ctx = MockNativeContext::new();
        let bound = untyped(&mut ctx, "p/C", CAT, MH_KIND_STATIC);
        let dash = Value::Object(Some(ctx.create_string("-")));
        ctx.set_field(bound, MH_BOUND, dash);
        let declared = Some(BOUND_CAT);
        match bound_handle_arity_refusal(&mut ctx, bound, SPREAD_SITE, declared) {
            Some(refusal) => assert!(is_wmte(&ctx, &refusal), "{refusal:?}"),
            None => panic!("a two-argument site over a one-parameter bound handle"),
        }
        let generic = generic_method_type_descriptor(2);
        assert!(bound_handle_arity_refusal(&mut ctx, bound, &generic, declared).is_some());
        // The same arity (a cast, not this refusal's business), and the exact type.
        let object_site = "(Ljava/lang/Object;)Ljava/lang/String;";
        assert!(bound_handle_arity_refusal(&mut ctx, bound, object_site, declared).is_none());
        assert!(bound_handle_arity_refusal(&mut ctx, bound, BOUND_CAT, declared).is_none());
        // A `type` that is not the leaf minus the capture is never judged.
        let unnarrowed = Some("()Ljava/lang/String;");
        assert!(bound_handle_arity_refusal(&mut ctx, bound, SPREAD_SITE, unnarrowed).is_none());
        assert!(bound_handle_arity_refusal(&mut ctx, bound, SPREAD_SITE, None).is_none());
        // A collector (`bindTo(..).asVarargsCollector(..)`) collects.
        mh_set_varargs_collector(&ctx, bound, true);
        assert!(bound_handle_arity_refusal(&mut ctx, bound, SPREAD_SITE, declared).is_none());
        mh_set_varargs_collector(&ctx, bound, false);
        // An unbound handle is `mh_entry_adapt`'s (static) or not judged here.
        ctx.set_field(bound, MH_BOUND, Value::Object(None));
        assert!(bound_handle_arity_refusal(&mut ctx, bound, SPREAD_SITE, declared).is_none());
        // A bound virtual handle's `MH_DESC` has no receiver to subtract.
        let fmt = "([Ljava/lang/Object;)Ljava/lang/String;";
        let virt = untyped(&mut ctx, "java/lang/String", fmt, MH_KIND_VIRTUAL);
        ctx.set_field(virt, MH_BOUND, dash);
        let two = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/String;";
        assert!(bound_handle_arity_refusal(&mut ctx, virt, two, Some(fmt)).is_some());
        assert!(bound_handle_arity_refusal(&mut ctx, virt, fmt, Some(fmt)).is_none());
        // An UNBOUND virtual handle: its `type` leads with the receiver.
        let one = "(Ljava/lang/String;)Ljava/lang/String;";
        let concat = untyped(&mut ctx, "java/lang/String", one, MH_KIND_VIRTUAL);
        let own = "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;";
        let three = "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;";
        let on = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_INVOKE_VIRTUAL_ARITY");
        assert_eq!(
            bound_handle_arity_refusal(&mut ctx, concat, three, Some(own)).is_some(),
            on
        );
        assert!(bound_handle_arity_refusal(&mut ctx, concat, own, Some(own)).is_none());
        // A receiver-less `type` (not the leaf plus the receiver) is not judged.
        assert!(bound_handle_arity_refusal(&mut ctx, concat, three, Some(one)).is_none());
    }

    /// `asType`'s raw-descriptor escape reads a bound static handle's
    /// `MH_DESC` without the captured parameter, so a longer type is refused
    /// (HotSpot: `cannot convert MethodHandle(String[])String to
    /// (String,String)String`) where the capture made the arity match.
    #[test]
    fn a_bound_static_raw_type_drops_the_capture() {
        let mut ctx = MockNativeContext::new();
        let bound = untyped(&mut ctx, "p/C", CAT, MH_KIND_STATIC);
        assert_eq!(
            bound_static_raw_desc(&ctx, bound, CAT),
            None,
            "not bound yet"
        );
        let dash = Value::Object(Some(ctx.create_string("-")));
        ctx.set_field(bound, MH_BOUND, dash);
        let on = mh_bindto_fixed_arity_enabled();
        assert_eq!(
            bound_static_raw_desc(&ctx, bound, CAT),
            on.then(|| BOUND_CAT.to_string())
        );
        if on {
            let refusal = mh_astype_refusal_desc(&mut ctx, bound, BOUND_CAT, SPREAD_SITE);
            assert!(
                refusal.as_ref().is_some_and(|r| is_wmte(&ctx, r)),
                "{refusal:?}"
            );
            // The same arity still converts.
            let object_site = "(Ljava/lang/Object;)Ljava/lang/String;";
            assert!(mh_astype_refusal_desc(&mut ctx, bound, BOUND_CAT, object_site).is_none());
        }
    }

    /// Which kinds `bindTo` hands to the INSERT adapter: every kind whose
    /// dispatch does not read an object in `MH_BOUND` as the captured leading
    /// argument, and a second bind of the direct kinds.
    #[test]
    fn the_insert_adapter_takes_the_kinds_the_copy_gets_wrong() {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_BINDTO_INSERT_ADAPTER") {
            return;
        }
        let instance_setter = "(Lp/H;Ljava/lang/String;)V";
        let static_setter = "(Ljava/lang/String;)V";
        let direct_like = [
            MH_KIND_STATIC,
            MH_KIND_VIRTUAL,
            MH_KIND_GETTER,
            MH_KIND_IDENTITY,
            MH_KIND_ARRAY_GET,
        ];
        for kind in direct_like {
            assert!(
                !bindto_takes_insert_adapter(kind, false, CAT),
                "kind {kind} unbound"
            );
            assert!(
                bindto_takes_insert_adapter(kind, true, CAT),
                "kind {kind} bound"
            );
        }
        assert!(!bindto_takes_insert_adapter(
            MH_KIND_SETTER,
            false,
            instance_setter
        ));
        assert!(bindto_takes_insert_adapter(
            MH_KIND_SETTER,
            true,
            instance_setter
        ));
        assert!(bindto_takes_insert_adapter(
            MH_KIND_SETTER,
            false,
            static_setter
        ));
        for kind in [MH_KIND_CONSTANT, MH_KIND_LAMBDA_FACTORY] {
            assert!(
                !bindto_takes_insert_adapter(kind, false, CAT),
                "kind {kind}"
            );
            assert!(
                !bindto_takes_insert_adapter(kind, true, CAT),
                "kind {kind} bound"
            );
        }
        for kind in [
            MH_KIND_CONSTRUCTOR,
            MH_KIND_INVOKER,
            MH_KIND_INSERT,
            MH_KIND_DROP,
            MH_KIND_FILTER,
            MH_KIND_GUARD,
            MH_KIND_COLLECT,
            MH_KIND_SPREAD,
            MH_KIND_DYNAMIC_INVOKER,
        ] {
            assert!(bindto_takes_insert_adapter(kind, false, CAT), "kind {kind}");
        }
    }

    /// End to end on an adapter: `bindTo` on an INSERT handle wraps it (the
    /// wrapper's target is the handle itself) instead of overwriting its
    /// wrapper with the bound value; the inner handle is left as it was.
    #[test]
    fn binding_an_adapter_wraps_it_and_leaves_its_wrapper_alone() {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_BINDTO_INSERT_ADAPTER") {
            return;
        }
        let mut ctx = MockNativeContext::new();
        let inner = untyped(&mut ctx, "__adapter__", CAT, MH_KIND_INSERT);
        let inner_wrapper = Value::Object(Some(ctx.create_string("inner wrapper")));
        ctx.set_field(inner, MH_BOUND, inner_wrapper);
        let dash = Value::Object(Some(ctx.create_string("-")));
        let bound = bind_to(&mut ctx, inner, dash);
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
        assert_ne!(bound, inner);
        assert_eq!(ctx.get_field(bound, MH_KIND), Value::Int(MH_KIND_INSERT));
        assert_eq!(
            ctx.get_field(inner, MH_BOUND),
            inner_wrapper,
            "the inner wrapper is untouched"
        );
        let wrapper = match ctx.get_field(bound, MH_BOUND) {
            Value::Object(Some(w)) => w,
            other => panic!("no insert wrapper: {other:?}"),
        };
        assert_eq!(ctx.get_field(wrapper, 0), Value::Object(Some(inner)));
        assert_eq!(ctx.get_field(wrapper, 2), Value::Int(0));
        assert!(!mh_is_varargs_collector(&ctx, bound));
    }
}

/// Round 15 wave 5, lane mh4: the adapter kinds' arity refusal
/// (`r15w4-mh3-adapter-invoke-has-no-arity-refusal-FIXED-20260930.md`, HotSpot ref
/// `R15Mh3AdapterArity`), `insertArguments` with no values answering its
/// target, and the bound static owner slot
/// (`r15w4-mh3-bindto-static-drops-recorded-owner-FIXED-20260930.md`). The handles
/// carry no `type`, as in `r15w4_mh3_bindto_tests`; `declared` is passed.
#[cfg(test)]
mod r15w5_mh4_adapter_arity_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    const CAT: &str = "(Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/String;";
    const BOUND_CAT: &str = "([Ljava/lang/String;)Ljava/lang/String;";
    const TWO: &str = "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;";
    const ONE: &str = "(Ljava/lang/String;)Ljava/lang/String;";
    const THREE: &str =
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;";

    fn untyped(ctx: &mut MockNativeContext, class: &str, desc: &str, kind: i32) -> ObjectRef {
        let mh = match alloc_method_handle(ctx, class, "m", desc, kind) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc {class}: {e:?}"),
        };
        ctx.set_field_by_name(mh, "type", Value::Object(None));
        mh
    }

    fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
        match err {
            MethodCallFailed::ExceptionThrown(o) => {
                ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                    == Some("java/lang/invoke/WrongMethodTypeException")
            }
            _ => false,
        }
    }

    /// MH3-3: every `MH_KIND_*` this model mints (0 ..= `MH_KIND_TABLE_SWITCH`,
    /// contiguous) states what `MH_BOUND` holds; the field accessors by form.
    #[test]
    fn every_kind_has_a_bound_role() {
        for kind in 0..=MH_KIND_TABLE_SWITCH {
            assert!(
                mh_bound_role(kind, "()V").is_some(),
                "kind {kind} has no role"
            );
        }
        assert_eq!(mh_bound_role(MH_KIND_TABLE_SWITCH + 1, "()V"), None);
        assert_eq!(mh_bound_role(-1, "()V"), None);
        assert_eq!(mh_bound_role(MH_KIND_GETTER, "()I"), Some(BoundRole::Owner));
        assert_eq!(
            mh_bound_role(MH_KIND_GETTER, "(Lp/H;)I"),
            Some(BoundRole::Capture)
        );
        assert_eq!(
            mh_bound_role(MH_KIND_SETTER, "(I)V"),
            Some(BoundRole::Owner)
        );
        assert_eq!(
            mh_bound_role(MH_KIND_SETTER, "(Lp/H;I)V"),
            Some(BoundRole::Capture)
        );
        assert_eq!(
            mh_bound_role(MH_KIND_CONSTRUCTOR, CAT),
            Some(BoundRole::Owner)
        );
        assert_eq!(mh_bound_role(MH_KIND_STATIC, CAT), Some(BoundRole::Capture));
        assert_eq!(mh_bound_role(MH_KIND_INSERT, CAT), Some(BoundRole::Wrapper));
        assert_eq!(
            mh_bound_role(MH_KIND_CONSTANT, "()I"),
            Some(BoundRole::Value)
        );
        assert_eq!(
            mh_bound_role(MH_KIND_LAMBDA_FACTORY, CAT),
            Some(BoundRole::Captures)
        );
        assert_eq!(
            mh_bound_role(MH_KIND_DYNAMIC_INVOKER, CAT),
            Some(BoundRole::CallSite)
        );
        assert_eq!(mh_bound_role(MH_KIND_INVOKER, CAT), Some(BoundRole::Unused));
        // Every adapter kind the arity refusal judges is a wrapper.
        for kind in 0..=MH_KIND_TABLE_SWITCH {
            if mh_adapter_arity_kind(kind) {
                assert_eq!(
                    mh_bound_role(kind, CAT),
                    Some(BoundRole::Wrapper),
                    "kind {kind}"
                );
            }
        }
    }

    /// The kinds judged: every measured adapter kind, no direct member kind.
    #[test]
    fn the_adapter_kinds_are_the_measured_combinators() {
        for kind in [
            MH_KIND_INSERT,
            MH_KIND_DROP,
            MH_KIND_FILTER,
            MH_KIND_COLLECT,
            MH_KIND_SPREAD,
        ] {
            assert!(mh_adapter_arity_kind(kind), "kind {kind}");
        }
        for kind in [
            MH_KIND_STATIC,
            MH_KIND_VIRTUAL,
            MH_KIND_SPECIAL,
            MH_KIND_CONSTRUCTOR,
            MH_KIND_INVOKER,
            MH_KIND_IDENTITY,
            MH_KIND_CONSTANT,
        ] {
            assert!(!mh_adapter_arity_kind(kind), "kind {kind}");
        }
    }

    /// `insertArguments(cat, 0, "-").invoke("a", "b")`, `filterArguments(cat2,
    /// 0, upper).invoke("a")`, `dropArguments(cat2, 0, String).invoke(q, a, b,
    /// c)`: another arity is refused; the same arity, a collector, an unknown
    /// or `()V` type are not judged.
    #[test]
    fn an_adapter_refuses_another_arity() {
        if !mh_invoke_adapter_arity_enabled() || !mh_strict_exact_signature() {
            return;
        }
        let mut ctx = MockNativeContext::new();
        let insert = untyped(&mut ctx, "__adapter__", CAT, MH_KIND_INSERT);
        let wrapper = Value::Object(Some(ctx.create_string("wrapper")));
        ctx.set_field(insert, MH_BOUND, wrapper);
        match bound_handle_arity_refusal(&mut ctx, insert, TWO, Some(BOUND_CAT)) {
            Some(refusal) => assert!(is_wmte(&ctx, &refusal), "{refusal:?}"),
            None => panic!("a two-argument site over a one-parameter INSERT adapter"),
        }
        let generic = generic_method_type_descriptor(2);
        assert!(bound_handle_arity_refusal(&mut ctx, insert, &generic, Some(BOUND_CAT)).is_some());
        // Its own type, and the same arity (a cast, not this refusal's business).
        assert!(bound_handle_arity_refusal(&mut ctx, insert, BOUND_CAT, Some(BOUND_CAT)).is_none());
        let object_site = "(Ljava/lang/Object;)Ljava/lang/String;";
        assert!(
            bound_handle_arity_refusal(&mut ctx, insert, object_site, Some(BOUND_CAT)).is_none()
        );
        // No `type`, or C21's `()V` stand-in: not judged.
        assert!(bound_handle_arity_refusal(&mut ctx, insert, TWO, None).is_none());
        assert!(bound_handle_arity_refusal(&mut ctx, insert, TWO, Some("()V")).is_none());
        // An explicit collector (`asVarargsCollector` of the adapter) collects.
        mh_set_varargs_collector(&ctx, insert, true);
        assert!(bound_handle_arity_refusal(&mut ctx, insert, TWO, Some(BOUND_CAT)).is_none());
        mh_set_varargs_collector(&ctx, insert, false);
        // FILTER one short, DROP one long; the leaf's `MH_DESC` is not consulted.
        let filter = untyped(&mut ctx, "__adapter__", TWO, MH_KIND_FILTER);
        assert!(bound_handle_arity_refusal(&mut ctx, filter, ONE, Some(TWO)).is_some());
        assert!(bound_handle_arity_refusal(&mut ctx, filter, TWO, Some(TWO)).is_none());
        let dropper = untyped(&mut ctx, "0:1", THREE, MH_KIND_DROP);
        let four = "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)\
                    Ljava/lang/String;";
        assert!(bound_handle_arity_refusal(&mut ctx, dropper, four, Some(THREE)).is_some());
        assert!(bound_handle_arity_refusal(&mut ctx, dropper, THREE, Some(THREE)).is_none());
    }

    /// Memory safety, independent of every switch: a leaf is never entered
    /// with fewer arguments than its descriptor has parameters (the VM would
    /// read the missing one as the uninitialized-local marker; the w4a
    /// `R15Mh3AdapterArity` `filter-short` access violation). A surplus still
    /// passes (the VM widens the frame for it).
    #[test]
    fn a_leaf_is_never_entered_short() {
        let mut ctx = MockNativeContext::new();
        let a = Value::Object(Some(ctx.create_string("a")));
        let b = Value::Object(Some(ctx.create_string("b")));
        match adapt_leaf_args(&mut ctx, &[a], TWO, None, false) {
            Err(e) => assert!(is_wmte(&ctx, &e), "{e:?}"),
            Ok(v) => panic!("a one-argument list reached a two-parameter leaf: {v:?}"),
        }
        assert!(adapt_leaf_args(&mut ctx, &[], ONE, None, false).is_err());
        assert!(adapt_leaf_args(&mut ctx, &[a, b], TWO, None, false).is_ok());
        assert!(adapt_leaf_args(&mut ctx, &[a, b, a], TWO, None, false).is_ok());
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
        // End to end, the crash's shape: `filterArguments(cat2, 0, <none>)`
        // dispatched with ONE argument (the doors' refusals bypassed, as with
        // `CRATONVM_MH_INVOKE_ADAPTER_ARITY=0`) reaches the static leaf short.
        let cat2 = untyped(&mut ctx, "p/C", TWO, MH_KIND_STATIC);
        let wrapper = alloc_mh_carrier_holding(
            &mut ctx,
            "__mh_filter_wrapper__",
            &[
                Value::Object(Some(cat2)),
                Value::Object(None),
                Value::Int(0),
            ],
        );
        let filter = untyped(&mut ctx, "__adapter__", TWO, MH_KIND_FILTER);
        ctx.set_field(filter, MH_BOUND, Value::Object(Some(wrapper)));
        match mh_dispatch(&mut ctx, filter, &[a]) {
            Err(e) => assert!(is_wmte(&ctx, &e), "{e:?}"),
            Ok(v) => panic!("the FILTER adapter entered its leaf one argument short: {v:?}"),
        }
    }

    /// Memory safety (upcall1's patch page): a RAW primitive at a leaf's
    /// REFERENCE parameter is boxed (`asType`'s boxing conversion), never laid
    /// down as an `int` in a reference local; the site's primitive picks the
    /// box when it names one, the raw variant otherwise. Every pin released.
    #[test]
    fn a_raw_primitive_at_a_reference_parameter_is_boxed() {
        let mut ctx = MockNativeContext::new();
        // The boxes are CANONICAL (`box_value_canonical`: the process-wide
        // `valueOf` caches keyed by `vm_identity()`). Under the shared mock
        // identity `0` this test both read boxes another test's dropped heap
        // left there and left its own for the next test to read (round 15
        // wave 6, lane mh5: `invalid ObjectRef in mock heap` here and in
        // `a_reference_index_follows_unbox_widen`). A private identity.
        ctx.set_vm_identity(0x15F5_0001);
        let boxed_desc = "(Ljava/lang/Integer;Ljava/lang/Integer;)Ljava/lang/Integer;";
        let site = vec!["I".to_string(), "I".to_string()];
        let adapted = match adapt_leaf_args(
            &mut ctx,
            &[Value::Int(7), Value::Int(9)],
            boxed_desc,
            Some(site.as_slice()),
            false,
        ) {
            Ok(v) => v,
            Err(e) => panic!("boxing refused: {e:?}"),
        };
        assert_eq!(adapted.len(), 2);
        for (v, want) in adapted.iter().zip([7, 9]) {
            match *v {
                Value::Object(Some(o)) => {
                    assert_eq!(crate::lang_class::unbox_value(&ctx, o), Value::Int(want));
                }
                other => panic!("a raw primitive reached a reference parameter: {other:?}"),
            }
        }
        // No site: the raw variant's wrapper (a `long` is a `Long`).
        match adapt_leaf_args(
            &mut ctx,
            &[Value::Long(3)],
            "(Ljava/lang/Object;)V",
            None,
            false,
        ) {
            Ok(v) => assert!(matches!(v.as_slice(), [Value::Object(Some(_))]), "{v:?}"),
            Err(e) => panic!("{e:?}"),
        }
        // A primitive parameter keeps its raw value; a reference keeps its object.
        let s = Value::Object(Some(ctx.create_string("s")));
        match adapt_leaf_args(
            &mut ctx,
            &[Value::Int(1), s],
            "(ILjava/lang/String;)V",
            None,
            false,
        ) {
            Ok(v) => assert_eq!(v, vec![Value::Int(1), s]),
            Err(e) => panic!("{e:?}"),
        }
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    /// `mh_dispatch_at_site` arms what the `invoke` door arms: the site
    /// without the receiver for an unbound virtual / special handle only.
    #[test]
    fn the_upcall_site_is_receiver_stripped_for_an_unbound_virtual_handle() {
        let mut ctx = MockNativeContext::new();
        let concat = untyped(&mut ctx, "java/lang/String", ONE, MH_KIND_VIRTUAL);
        assert_eq!(
            mh_receiver_stripped_site(&ctx, concat, TWO).as_deref(),
            Some(ONE)
        );
        let dash = Value::Object(Some(ctx.create_string("-")));
        ctx.set_field(concat, MH_BOUND, dash);
        assert_eq!(
            mh_receiver_stripped_site(&ctx, concat, ONE),
            None,
            "a bound receiver"
        );
        let cat2 = untyped(&mut ctx, "p/C", TWO, MH_KIND_STATIC);
        assert_eq!(
            mh_receiver_stripped_site(&ctx, cat2, TWO),
            None,
            "a static handle"
        );
    }

    /// `invoke`'s `asType(site)` refuses a wrapper that cannot unbox-and-widen
    /// to a primitive parameter (and any other reference/primitive pair
    /// `MethodType.canConvert` refuses) with WMTE, before the argument casts:
    /// `R15Mh2InvokeExactType` `null-order row3` (a `Long` at an `int`).
    #[test]
    fn a_boxed_argument_that_cannot_convert_is_the_types_refusal() {
        if !mh_strict_exact_signature()
            || !cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_MH_INVOKE_BOXED_ARG_REFUSAL",
            )
        {
            return;
        }
        let mut ctx = MockNativeContext::new();
        let m = untyped(&mut ctx, "java/lang/String", "(I)I", MH_KIND_VIRTUAL);
        let declared = Some("(Ljava/lang/String;I)I");
        let site = |p: &str| format!("(Ljava/lang/String;{p})I");
        for refused in [
            "Ljava/lang/Long;",
            "Ljava/lang/String;",
            "Ljava/lang/Boolean;",
            "[I",
        ] {
            match invoke_narrowing_arg_refusal(
                &mut ctx,
                m,
                MH_KIND_VIRTUAL,
                &site(refused),
                declared,
            ) {
                Some(e) => assert!(is_wmte(&ctx, &e), "{refused}: {e:?}"),
                None => panic!("{refused} at an int parameter must be asType's refusal"),
            }
        }
        for converts in [
            "Ljava/lang/Integer;",
            "Ljava/lang/Short;",
            "Ljava/lang/Character;",
            "Ljava/lang/Number;",
            "Ljava/lang/Object;",
            "Ljava/lang/Comparable;",
            "S",
            "I",
        ] {
            assert!(
                invoke_narrowing_arg_refusal(
                    &mut ctx,
                    m,
                    MH_KIND_VIRTUAL,
                    &site(converts),
                    declared
                )
                .is_none(),
                "{converts} converts to int"
            );
        }
        // A variable-arity collector COLLECTS at its trailing array position
        // (hand-back 3: `SHOW(Object[]).invoke(7)` is "1:Integer" on HotSpot),
        // so that position is judged against the component, not the array.
        let show_desc = "([Ljava/lang/Object;)Ljava/lang/String;";
        let show = untyped(&mut ctx, "p/C", show_desc, MH_KIND_STATIC);
        mh_set_varargs_collector(&ctx, show, true);
        for collected in ["I", "J", "Ljava/lang/String;", "[I"] {
            let s = format!("({collected})Ljava/lang/String;");
            assert!(
                invoke_narrowing_arg_refusal(&mut ctx, show, MH_KIND_STATIC, &s, Some(show_desc))
                    .is_none(),
                "{collected} is collected into the Object[]"
            );
        }
        let ints_desc = "([I)V";
        let ints = untyped(&mut ctx, "p/C", ints_desc, MH_KIND_STATIC);
        mh_set_varargs_collector(&ctx, ints, true);
        assert!(invoke_narrowing_arg_refusal(
            &mut ctx,
            ints,
            MH_KIND_STATIC,
            "(I)V",
            Some(ints_desc)
        )
        .is_none());
        assert!(invoke_narrowing_arg_refusal(
            &mut ctx,
            ints,
            MH_KIND_STATIC,
            "(J)V",
            Some(ints_desc)
        )
        .is_some_and(|e| is_wmte(&ctx, &e)));
        // Not a collector: an `int` at an `Object[]` parameter is refused.
        mh_set_varargs_collector(&ctx, show, false);
        assert!(invoke_narrowing_arg_refusal(
            &mut ctx,
            show,
            MH_KIND_STATIC,
            "(I)Ljava/lang/String;",
            Some(show_desc)
        )
        .is_some());
        // A primitive at a reference parameter: `int` boxes to `Integer`,
        // which is no `Long`.
        let takes_long = Some("(Ljava/lang/String;Ljava/lang/Long;)I");
        assert!(
            invoke_narrowing_arg_refusal(&mut ctx, m, MH_KIND_VIRTUAL, &site("I"), takes_long)
                .is_some()
        );
        assert!(
            invoke_narrowing_arg_refusal(&mut ctx, m, MH_KIND_VIRTUAL, &site("J"), takes_long)
                .is_none()
        );
    }

    /// `insertArguments(t, pos)` with no values is `t` itself (HotSpot's
    /// `if (insCount == 0) return target;`), so a virtual target keeps its
    /// receiver-leading `type` rather than an adapter typed by `MH_DESC`.
    #[test]
    fn inserting_no_values_answers_the_target() {
        let mut ctx = MockNativeContext::new();
        let target = untyped(&mut ctx, "java/lang/String", ONE, MH_KIND_VIRTUAL);
        let object = crate::lang_class::reflection_component_id(&mut ctx, "java/lang/Object");
        let empty = ctx.new_ref_array(object, 0);
        let mut registry = NativeMethodRegistry::new();
        register_method_handle_combinator_extras_bridge(&mut registry);
        let insert = registry
            .find(
                "java/lang/invoke/MethodHandles",
                "insertArguments",
                "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
            )
            .expect("MethodHandles.insertArguments should be registered");
        let answer = match insert(
            &mut ctx,
            &[
                Value::Object(Some(target)),
                Value::Int(0),
                Value::Object(Some(empty)),
            ],
        ) {
            Ok(Some(Value::Object(Some(h)))) => h,
            other => panic!("insertArguments answered {other:?}"),
        };
        assert_eq!(answer == target, mh_invoke_adapter_arity_enabled());
    }

    /// `findStatic(C, ..).bindTo(x)` keeps the owner the unbound handle
    /// recorded in `MH_BOUND` (the bound copy's `MH_BOUND` is `x`).
    #[test]
    fn a_bound_static_handle_keeps_its_recorded_owner() {
        let mut ctx = MockNativeContext::new();
        let cat = untyped(&mut ctx, "p/C", CAT, MH_KIND_STATIC);
        assert_eq!(
            ctx.get_field(cat, MH_STATIC_OWNER),
            Value::Int(0),
            "minted empty"
        );
        assert_eq!(
            mh_static_owner_in(&ctx, cat, ctx.get_field(cat, MH_BOUND)),
            None
        );
        mh_record_static_owner(&mut ctx, cat, Some(ClassId::new(7)));
        assert_eq!(
            mh_static_owner_in(&ctx, cat, ctx.get_field(cat, MH_BOUND)),
            Some(ClassId::new(7))
        );
        let dash = Value::Object(Some(ctx.create_string("-")));
        let mut registry = NativeMethodRegistry::new();
        register_t4_method_handle_invoke(&mut registry);
        let bind = registry
            .find(
                "java/lang/invoke/MethodHandle",
                "bindTo",
                "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
            )
            .expect("MethodHandle.bindTo should be registered");
        let bound = match bind(&mut ctx, &[Value::Object(Some(cat)), dash]) {
            Ok(Some(Value::Object(Some(h)))) => h,
            other => panic!("bindTo answered {other:?}"),
        };
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
        assert_eq!(ctx.get_field(bound, MH_KIND), Value::Int(MH_KIND_STATIC));
        assert_eq!(ctx.get_field(bound, MH_BOUND), dash);
        let expect = mh_bindto_static_owner_enabled().then(|| ClassId::new(7));
        assert_eq!(mh_static_owner_in(&ctx, bound, dash), expect);
        // An `asType` / `asVarargsCollector` copy keeps it.
        let copy = match mh_clone_handle(&mut ctx, bound) {
            Some(c) => c,
            None => panic!("the bound handle is of this model's layout"),
        };
        assert_eq!(mh_static_owner_in(&ctx, copy, dash), expect);
    }
}

/// Round 15 wave 6, lane mh5: `asType` / `explicitCastArguments` of a
/// variable-arity handle answer the JDK's fixed-arity composition (MH4-1),
/// `insertArguments`' null arguments, and the capture-dropping raw descriptor
/// of every capturing kind (MH3-6). `MethodType`'s `rtype` / `ptypes` are
/// declared, as in `r13w10_callcost4_direct_lane_tests`, so a handle's `type`
/// reads back.
#[cfg(test)]
mod r15w6_mh5_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    const CAT: &str = "(Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/String;";
    const THREE: &str =
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;";

    fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
        match build_method_type_from_descriptor(ctx, desc) {
            Ok(Some(mt)) => mt,
            other => panic!("MethodType {desc}: {other:?}"),
        }
    }

    fn setup() -> MockNativeContext {
        let mut ctx = MockNativeContext::new();
        let _ = ctx.ensure_class_initialized("p/C");
        let probe = mt(&mut ctx, "()V");
        let cid = ctx.class_id_of_object(probe);
        let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            cid,
            vec![
                field("rtype", "Ljava/lang/Class;", 0),
                field("ptypes", "[Ljava/lang/Class;", 1),
            ],
        );
        ctx
    }

    fn handle(ctx: &mut MockNativeContext, desc: &str, kind: i32) -> ObjectRef {
        match alloc_method_handle(ctx, "p/C", "m", desc, kind) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc: {e:?}"),
        }
    }

    fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
        match err {
            MethodCallFailed::ExceptionThrown(o) => {
                ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                    == Some("java/lang/invoke/WrongMethodTypeException")
            }
            _ => false,
        }
    }

    fn answered_handle(r: Option<MethodCallResult>) -> ObjectRef {
        match r {
            Some(Ok(Some(Value::Object(Some(h))))) => h,
            other => panic!("answered {other:?}"),
        }
    }

    /// `asCollector(Class, int)` answered by its registered native, as the VM
    /// would dispatch it.
    fn as_collector_hook(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        name: &str,
        desc: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        if name != "asCollector" || desc != MH_AS_COLLECTOR_DESC {
            return None;
        }
        let mut registry = NativeMethodRegistry::new();
        register_method_handle_combinator_extras_bridge(&mut registry);
        let native = registry.find(
            "java/lang/invoke/MethodHandle",
            "asCollector",
            MH_AS_COLLECTOR_DESC,
        )?;
        let mut full = vec![Value::Object(Some(receiver))];
        full.extend_from_slice(args);
        Some(native(ctx, &full))
    }

    /// MH4-1: `cat.asType(t)` of a collector is fixed-arity (the receiver
    /// keeps collecting); at the same arity with the array trailing it is the
    /// fixed copy stamped, at another arity an `asCollector` adapter of the
    /// fixed copy; an element that does not convert to the component, and an
    /// arity below `collectArg`, are refused.
    #[test]
    fn the_astype_of_a_collector_is_fixed_arity() {
        if !mh_astype_fixed_collector_enabled() {
            return;
        }
        let mut ctx = setup();
        let cat = handle(&mut ctx, CAT, MH_KIND_STATIC);
        let three = mt(&mut ctx, THREE);
        // Not a collector: not this function's business.
        assert!(mh_astype_varargs_collector(&mut ctx, cat, three).is_none());
        mh_set_varargs_collector(&ctx, cat, true);
        // The receiver's own type: the receiver (the caller's stamp answers it).
        let own = mt(&mut ctx, CAT);
        assert!(mh_astype_varargs_collector(&mut ctx, cat, own).is_none());
        // Same arity, the array trailing: `asFixedArity().asType(t)`.
        let object_return = "(Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/Object;";
        let t = mt(&mut ctx, object_return);
        let fixed = answered_handle(mh_astype_varargs_collector(&mut ctx, cat, t));
        assert_ne!(fixed, cat);
        assert!(!mh_is_varargs_collector(&ctx, fixed), "a fixed-arity copy");
        assert!(
            mh_is_varargs_collector(&ctx, cat),
            "the receiver is untouched"
        );
        assert_eq!(ctx.get_field(fixed, MH_KIND), Value::Int(MH_KIND_STATIC));
        assert_eq!(
            mh_declared_descriptor(&mut ctx, fixed).as_deref(),
            Some(object_return)
        );
        // Another arity: `asFixedArity().asCollector(String[], 2).asType(t)`.
        ctx.set_invoke_virtual_hook(as_collector_hook);
        let three = mt(&mut ctx, THREE);
        let collector = answered_handle(mh_astype_varargs_collector(&mut ctx, cat, three));
        assert_eq!(
            ctx.get_field(collector, MH_KIND),
            Value::Int(MH_KIND_COLLECT)
        );
        assert!(!mh_is_varargs_collector(&ctx, collector));
        assert_eq!(
            mh_declared_descriptor(&mut ctx, collector).as_deref(),
            Some(THREE)
        );
        let wrapper = match ctx.get_field(collector, MH_BOUND) {
            Value::Object(Some(w)) => w,
            other => panic!("collector wrapper {other:?}"),
        };
        assert_eq!(ctx.get_field(wrapper, 1), Value::Int(2), "two collected");
        match ctx.get_field(wrapper, 0) {
            Value::Object(Some(inner)) => {
                assert_ne!(inner, cat);
                assert!(!mh_is_varargs_collector(&ctx, inner), "over the fixed copy");
            }
            other => panic!("collector target {other:?}"),
        }
        // `int` elements do not convert to `String`: the inner `asType`'s WMTE.
        let ints = mt(&mut ctx, "(Ljava/lang/String;II)Ljava/lang/String;");
        match mh_astype_varargs_collector(&mut ctx, cat, ints) {
            Some(Err(e)) => assert!(is_wmte(&ctx, &e), "{e:?}"),
            other => panic!("(String,int,int)String: {other:?}"),
        }
        // Below `collectArg`: left to the refusal ("cannot build collector").
        let none = mt(&mut ctx, "()Ljava/lang/String;");
        assert!(mh_astype_varargs_collector(&mut ctx, cat, none).is_none());
        match mh_astype_refusal(&mut ctx, cat, none) {
            Some(e) => assert!(is_wmte(&ctx, &e), "{e:?}"),
            None => panic!("a collector asked for no parameters"),
        }
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    /// MH4-1: `explicitCastArguments` of a collector is fixed-arity too; an
    /// unchanged type answers the collector itself.
    #[test]
    fn the_explicit_cast_of_a_collector_is_fixed_arity() {
        if !mh_astype_fixed_collector_enabled() {
            return;
        }
        let mut ctx = setup();
        let cat = handle(&mut ctx, CAT, MH_KIND_STATIC);
        mh_set_varargs_collector(&ctx, cat, true);
        let mut registry = NativeMethodRegistry::new();
        register_method_handle_combinator_extras_bridge(&mut registry);
        let explicit = registry
            .find(
                "java/lang/invoke/MethodHandles",
                "explicitCastArguments",
                "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
            )
            .expect("MethodHandles.explicitCastArguments should be registered");
        let t = mt(
            &mut ctx,
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        );
        let cast = match explicit(
            &mut ctx,
            &[Value::Object(Some(cat)), Value::Object(Some(t))],
        ) {
            Ok(Some(Value::Object(Some(h)))) => h,
            other => panic!("explicitCastArguments answered {other:?}"),
        };
        assert_ne!(cast, cat);
        assert!(!mh_is_varargs_collector(&ctx, cast));
        assert!(
            mh_is_explicit_cast(&ctx, cast),
            "the explicit marking is kept"
        );
        assert!(mh_is_varargs_collector(&ctx, cat));
        let own = mt(&mut ctx, CAT);
        // The mock mints a fresh Class mirror on every descriptor lookup.
        // Model the VM's canonical mirrors when asking for the same type.
        let original_type = match ctx.get_field_by_name(cat, "type") {
            Value::Object(Some(t)) => t,
            other => panic!("collector type {other:?}"),
        };
        for field in ["rtype", "ptypes"] {
            let value = ctx.get_field_by_name(original_type, field);
            ctx.set_field_by_name(own, field, value);
        }
        match explicit(
            &mut ctx,
            &[Value::Object(Some(cat)), Value::Object(Some(own))],
        ) {
            Ok(Some(Value::Object(Some(h)))) => {
                assert_eq!(h, cat);
                assert!(mh_is_varargs_collector(&ctx, h));
            }
            other => panic!("explicitCastArguments answered {other:?}"),
        }
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    /// `insertArguments(t, pos, (Object[]) null)` and `insertArguments(null,
    /// pos, values)`: the JDK body's two NPEs, `values` first.
    #[test]
    fn insert_arguments_refuses_a_null_values_or_target() {
        if !mh_insert_arguments_null_npe_enabled() {
            return;
        }
        let mut ctx = setup();
        let target = handle(&mut ctx, THREE, MH_KIND_STATIC);
        let object = crate::lang_class::reflection_component_id(&mut ctx, "java/lang/Object");
        let empty = ctx.new_ref_array(object, 0);
        let mut registry = NativeMethodRegistry::new();
        register_method_handle_combinator_extras_bridge(&mut registry);
        let insert = registry
            .find(
                "java/lang/invoke/MethodHandles",
                "insertArguments",
                "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
            )
            .expect("MethodHandles.insertArguments should be registered");
        let npe = |r: MethodCallResult| match r {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { message, .. },
            ))) => message,
            other => panic!("expected an NPE, got {other:?}"),
        };
        let values = Some("Cannot read the array length because \"values\" is null".to_string());
        let target_npe = Some(
            "Cannot invoke \"java.lang.invoke.MethodHandle.type()\" because \"target\" is null"
                .to_string(),
        );
        let none = Value::Object(None);
        assert_eq!(
            npe(insert(
                &mut ctx,
                &[Value::Object(Some(target)), Value::Int(0), none]
            )),
            values
        );
        assert_eq!(
            npe(insert(&mut ctx, &[none, Value::Int(0), none])),
            values,
            "values first"
        );
        assert_eq!(
            npe(insert(
                &mut ctx,
                &[none, Value::Int(0), Value::Object(Some(empty))]
            )),
            target_npe
        );
    }

    /// MH3-6: the raw descriptor of a BOUND capturing handle drops the
    /// capture for every kind whose `MH_DESC` names it, never for a virtual
    /// one (its receiver was never in `MH_DESC`) and never unbound.
    #[test]
    fn a_bound_capture_kinds_raw_type_drops_the_capture() {
        if !mh_astype_capture_raw_desc_enabled() {
            return;
        }
        let mut ctx = setup();
        let getter = handle(&mut ctx, "([II)I", MH_KIND_ARRAY_GET);
        assert_eq!(
            bound_capture_raw_desc(&ctx, getter, "([II)I"),
            None,
            "not bound yet"
        );
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);
        ctx.set_field(getter, MH_BOUND, Value::Object(Some(arr)));
        assert_eq!(
            bound_capture_raw_desc(&ctx, getter, "([II)I").as_deref(),
            Some("(I)I")
        );
        let field = handle(&mut ctx, "(Lp/C;)I", MH_KIND_GETTER);
        let recv = Value::Object(Some(ctx.create_string("r")));
        ctx.set_field(field, MH_BOUND, recv);
        assert_eq!(
            bound_capture_raw_desc(&ctx, field, "(Lp/C;)I").as_deref(),
            Some("()I")
        );
        let virt = handle(&mut ctx, "(I)I", MH_KIND_VIRTUAL);
        ctx.set_field(virt, MH_BOUND, recv);
        assert_eq!(
            bound_capture_raw_desc(&ctx, virt, "(I)I"),
            None,
            "a receiver"
        );
        // The escape no longer lets a type one parameter longer through.
        let longer = "(Ljava/lang/Object;I)I";
        let refusal = mh_astype_refusal_desc(&mut ctx, getter, "(I)I", longer);
        assert!(
            refusal.as_ref().is_some_and(|e| is_wmte(&ctx, e)),
            "{refusal:?}"
        );
    }
}

/// Round 13 wave 10, lane callcost4: the direct lane of the `invoke` /
/// `invokeExact` doors (`mh_direct_lane`). The mock has no by-name hierarchy
/// walk, so a POSITIVE reference mismatch is an array (the cast-chain tests'
/// convention); its static invokes answer `Ok(None)`, and a virtual call is
/// answered by the hook below. End-to-end rows: `R13Callcost4MhDirect`.
#[cfg(test)]
mod r13w10_callcost4_direct_lane_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn object(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
        let o = match ctx.new_object(class) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("new_object {class}: {other:?}"),
        };
        ctx.set_field(o, 0, payload);
        Value::Object(Some(o))
    }

    fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
        match build_method_type_from_descriptor(ctx, desc) {
            Ok(Some(mt)) => mt,
            other => panic!("MethodType {desc}: {other:?}"),
        }
    }

    /// Declare `MethodType`'s `rtype` / `ptypes` so `methodtype_to_descriptor`
    /// can read a handle's declared type back.
    fn setup() -> MockNativeContext {
        let mut ctx = MockNativeContext::new();
        for name in ["p/C", "p/Mid", "p/Sub"] {
            let _ = ctx.ensure_class_initialized(name);
        }
        let probe = mt(&mut ctx, "()V");
        let cid = ctx.class_id_of_object(probe);
        let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            cid,
            vec![
                field("rtype", "Ljava/lang/Class;", 0),
                field("ptypes", "[Ljava/lang/Class;", 1),
            ],
        );
        ctx
    }

    fn handle(ctx: &mut MockNativeContext, class: &str, desc: &str, kind: i32) -> ObjectRef {
        match alloc_method_handle(ctx, class, "m", desc, kind) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc {class}: {e:?}"),
        }
    }

    /// `m(I)I` answers its argument plus 100.
    fn plus_hundred(
        _ctx: &mut MockNativeContext,
        _receiver: ObjectRef,
        name: &str,
        desc: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        match (name, desc, args.first()) {
            ("m", "(I)I", Some(Value::Int(x))) => Some(Ok(Some(Value::Int(x + 100)))),
            _ => None,
        }
    }

    /// The int a door answered, raw or boxed.
    fn answered_int(ctx: &MockNativeContext, r: &Option<MethodCallResult>) -> Option<i32> {
        match r {
            Some(Ok(Some(Value::Int(v)))) => Some(*v),
            Some(Ok(Some(Value::Object(Some(o))))) => match crate::lang_class::unbox_value(ctx, *o)
            {
                Value::Int(v) => Some(v),
                _ => None,
            },
            _ => None,
        }
    }

    fn cce_target(r: &Option<MethodCallResult>) -> Option<String> {
        match r {
            Some(Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { message, .. },
            )))) => message.rsplit(" to ").next().map(str::to_string),
            _ => None,
        }
    }

    /// A `findVirtual` handle at its own exact type, through both doors: the
    /// leaf's answer, every pin released.
    #[test]
    fn a_direct_virtual_handle_at_its_own_type_answers_the_leaf() {
        let mut ctx = setup();
        let mh = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
        let recv = object(&mut ctx, "p/Mid", Value::Int(0));
        ctx.set_invoke_virtual_hook(plus_hundred);
        for door in [DirectLaneDoor::InvokeExact, DirectLaneDoor::Invoke] {
            let r = mh_direct_lane(
                &mut ctx,
                mh,
                &[recv, Value::Int(5)],
                Some("(Lp/Mid;I)I"),
                door,
            );
            assert_eq!(answered_int(&ctx, &r), Some(105), "{door:?}: {r:?}");
        }
        // The same handle bound to its receiver (`bindTo`'s shape: the value
        // in `MH_BOUND`, the leading parameter dropped from `type`).
        let bound = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
        ctx.set_field(bound, MH_BOUND, recv);
        let dropped = mt(&mut ctx, "(I)I");
        ctx.set_field_by_name(bound, "type", Value::Object(Some(dropped)));
        for door in [DirectLaneDoor::InvokeExact, DirectLaneDoor::Invoke] {
            let r = mh_direct_lane(&mut ctx, bound, &[Value::Int(7)], Some("(I)I"), door);
            assert_eq!(answered_int(&ctx, &r), Some(107), "bound {door:?}: {r:?}");
        }
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    /// A static handle: the mock's static invoke answers `Ok(None)`, which the
    /// lane hands back exactly as `auto_box_return` does for a primitive
    /// return, and a `void` return is the door's `null`. The settle pin is
    /// released.
    #[test]
    fn a_direct_static_handle_answers_as_the_door_boxes() {
        let mut ctx = setup();
        let int_mh = handle(&mut ctx, "p/C", "(IJ)I", MH_KIND_STATIC);
        let r = mh_direct_lane(
            &mut ctx,
            int_mh,
            &[Value::Int(1), Value::Long(2)],
            Some("(IJ)I"),
            DirectLaneDoor::InvokeExact,
        );
        assert!(matches!(r, Some(Ok(None))), "{r:?}");
        let void_mh = handle(&mut ctx, "p/C", "()V", MH_KIND_STATIC);
        for door in [DirectLaneDoor::InvokeExact, DirectLaneDoor::Invoke] {
            let r = mh_direct_lane(&mut ctx, void_mh, &[], Some("()V"), door);
            assert!(
                matches!(r, Some(Ok(Some(Value::Object(None))))),
                "{door:?}: {r:?}"
            );
        }
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    /// Everything the lane does not model is declined before it has done
    /// anything, so the ordinary door runs.
    #[test]
    fn the_lane_declines_every_shape_it_does_not_model() {
        let mut ctx = setup();
        let mh = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
        let recv = object(&mut ctx, "p/Mid", Value::Int(0));
        let boxed = object(&mut ctx, "java/lang/Integer", Value::Int(5));
        let exact = DirectLaneDoor::InvokeExact;
        let site = Some("(Lp/Mid;I)I");
        // No call site, another call site, a boxed argument, a null receiver.
        assert!(mh_direct_lane(&mut ctx, mh, &[recv, Value::Int(5)], None, exact).is_none());
        assert!(mh_direct_lane(
            &mut ctx,
            mh,
            &[recv, Value::Long(5)],
            Some("(Lp/Mid;J)I"),
            exact
        )
        .is_none());
        assert!(mh_direct_lane(&mut ctx, mh, &[recv, boxed], site, exact).is_none());
        assert!(mh_direct_lane(
            &mut ctx,
            mh,
            &[Value::Object(None), Value::Int(5)],
            site,
            exact
        )
        .is_none());
        // A variable-arity collector.
        let collector = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
        mh_set_varargs_collector(&ctx, collector, true);
        assert!(mh_direct_lane(&mut ctx, collector, &[recv, Value::Int(5)], site, exact).is_none());
        // An `asType` copy: its `type` is not its member's own.
        let t = mt(&mut ctx, "(Ljava/lang/Object;I)I");
        let copy = match mh_with_stamped_type(&mut ctx, Value::Object(Some(mh)), t, false) {
            Ok(Some(Value::Object(Some(copy)))) => copy,
            other => panic!("stamp: {other:?}"),
        };
        assert!(mh_direct_lane(
            &mut ctx,
            copy,
            &[recv, Value::Int(5)],
            Some("(Ljava/lang/Object;I)I"),
            exact
        )
        .is_none());
        // An array parameter (the varargs repair's territory).
        let arr_mh = handle(&mut ctx, "p/C", "([I)I", MH_KIND_STATIC);
        let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
        assert!(mh_direct_lane(&mut ctx, arr_mh, &[arr], Some("([I)I"), exact).is_none());
        // A kind the lane does not take.
        let getter = handle(&mut ctx, "p/C", "()I", MH_KIND_GETTER);
        assert!(mh_direct_lane(&mut ctx, getter, &[], Some("()I"), exact).is_none());
        // A static handle with a bound leading argument.
        let static_bound = handle(&mut ctx, "p/C", "(Ljava/lang/Object;)I", MH_KIND_STATIC);
        ctx.set_field(static_bound, MH_BOUND, recv);
        assert!(mh_direct_lane(&mut ctx, static_bound, &[], Some("()I"), exact).is_none());
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "nothing pinned by a decline"
        );
    }

    /// A reference the leaf would refuse is refused by the lane with the
    /// ordinary door's own refusal: `invokeExact` with the leaf's
    /// `ClassCastException`; `invoke` on a static handle earlier, with
    /// `mh_entry_adapt`'s entry cast (a thrown Java exception), as the door.
    #[test]
    fn a_leaf_cast_is_refused_as_the_door_refuses_it() {
        let mut ctx = setup();
        let mh = handle(&mut ctx, "p/C", "(Lp/Sub;)I", MH_KIND_STATIC);
        let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
        let ordinary = adapt_leaf_args(&mut ctx, &[arr], "(Lp/Sub;)I", None, false);
        assert!(
            ordinary.is_err(),
            "the leaf refuses an array at a class parameter"
        );
        let exact = mh_direct_lane(
            &mut ctx,
            mh,
            &[arr],
            Some("(Lp/Sub;)I"),
            DirectLaneDoor::InvokeExact,
        );
        assert_eq!(cce_target(&exact).as_deref(), Some("p.Sub"), "{exact:?}");
        let entry = mh_entry_adapt(&mut ctx, mh, "(Lp/Sub;)I", "(Lp/Sub;)I", false, &[arr]);
        assert!(entry.is_err(), "the entry cast refuses it too");
        let invoke = mh_direct_lane(
            &mut ctx,
            mh,
            &[arr],
            Some("(Lp/Sub;)I"),
            DirectLaneDoor::Invoke,
        );
        assert!(matches!(invoke, Some(Err(_))), "{invoke:?}");
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    /// The explicit-cast scope of an enclosing dispatch is cleared for the
    /// lane's own call and restored on every exit, as the door does.
    #[test]
    fn the_explicit_scope_is_restored_on_every_exit() {
        let mut ctx = setup();
        let mh = handle(&mut ctx, "p/C", "()V", MH_KIND_STATIC);
        MH_EXPLICIT_SCOPE.with(|f| f.set(true));
        let answered = mh_direct_lane(&mut ctx, mh, &[], Some("()V"), DirectLaneDoor::InvokeExact);
        assert!(answered.is_some());
        assert!(
            MH_EXPLICIT_SCOPE.with(|f| f.get()),
            "restored after an answer"
        );
        let refused = handle(&mut ctx, "p/C", "(Lp/Sub;)V", MH_KIND_STATIC);
        let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
        let r = mh_direct_lane(
            &mut ctx,
            refused,
            &[arr],
            Some("(Lp/Sub;)V"),
            DirectLaneDoor::InvokeExact,
        );
        assert!(matches!(r, Some(Err(_))), "{r:?}");
        assert!(
            MH_EXPLICIT_SCOPE.with(|f| f.get()),
            "restored after a refusal"
        );
        MH_EXPLICIT_SCOPE.with(|f| f.set(false));
    }

    /// The raw return keeps a primitive of the return's own variant and boxes
    /// nothing else differently from `auto_box_return`.
    #[test]
    fn the_raw_return_is_taken_only_for_the_returns_own_variant() {
        let mut ctx = setup();
        let raw = mh_direct_lane_return(&mut ctx, Ok(Some(Value::Long(7))), "()J");
        if mh_direct_lane_raw_return_enabled() {
            assert_eq!(raw.ok(), Some(Some(Value::Long(7))));
        }
        // A mismatched variant is `auto_box_return`'s.
        let mismatched = mh_direct_lane_return(&mut ctx, Ok(Some(Value::Int(7))), "()J");
        assert!(
            !matches!(mismatched, Ok(Some(Value::Int(_)))),
            "{mismatched:?}"
        );
        // `void` and a missing reference are the door's `null`.
        assert!(matches!(
            mh_direct_lane_return(&mut ctx, Ok(None), "()V"),
            Ok(Some(Value::Object(None)))
        ));
        assert!(matches!(
            mh_direct_lane_return(&mut ctx, Ok(None), "()Ljava/lang/Object;"),
            Ok(Some(Value::Object(None)))
        ));
    }
}

/// JIT round 16 (lane mh): MH3-2, the per-VM bound `MethodType` table. The
/// mock context owns a `MethodTypeTable` exactly as the VM's class realm does.
#[cfg(test)]
mod r16_mh_bound_type_table_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    fn table_type(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
        match bound_method_type_through_table(ctx, desc, None) {
            Ok(Some(mt)) => mt,
            other => panic!("{desc}: {other:?}"),
        }
    }

    /// The proposal's test: two builds of one descriptor answer one object;
    /// the plain builder (the switch-off path) answers two.
    #[test]
    fn two_builds_of_one_descriptor_answer_one_object() {
        let mut ctx = MockNativeContext::new();
        for name in ["java/lang/String", "[I", "[Ljava/lang/String;"] {
            let _ = ctx.ensure_class_initialized(name);
        }
        for desc in [
            "(I)I",
            "()V",
            "(Ljava/lang/String;J)[I",
            "([Ljava/lang/String;)Z",
        ] {
            let a = table_type(&mut ctx, desc);
            let b = table_type(&mut ctx, desc);
            assert_eq!(a, b, "{desc}: one object");
        }
        let a = table_type(&mut ctx, "(I)I");
        let c = table_type(&mut ctx, "(J)I");
        assert_ne!(a, c, "distinct descriptors, distinct rows");
        let f1 = build_method_type_from_descriptor(&mut ctx, "(I)I")
            .unwrap()
            .unwrap();
        let f2 = build_method_type_from_descriptor(&mut ctx, "(I)I")
            .unwrap()
            .unwrap();
        assert_ne!(f1, f2, "the builder itself still mints");
    }

    /// Only java.*-and-primitive types that are already loaded are shared:
    /// an application class, an unloaded java.* class, a spun-looking name
    /// and an array of an application class each get a fresh object.
    #[test]
    fn only_loaded_java_and_primitive_types_are_shared() {
        let mut ctx = MockNativeContext::new();
        for name in ["java/lang/String", "app/Foo", "[D", "[Lapp/Foo;"] {
            let _ = ctx.ensure_class_initialized(name);
        }
        assert!(bound_method_type_shareable(&ctx, "(IJ[D)V"));
        assert!(bound_method_type_shareable(&ctx, "(Ljava/lang/String;)I"));
        assert!(!bound_method_type_shareable(&ctx, "(Lapp/Foo;)I"));
        assert!(!bound_method_type_shareable(&ctx, "()Lapp/Foo;"));
        assert!(!bound_method_type_shareable(&ctx, "([Lapp/Foo;)V"));
        assert!(!bound_method_type_shareable(
            &ctx,
            "(Ljava/util/NotLoadedHere;)V"
        ));
        assert!(!bound_method_type_shareable(
            &ctx,
            "(Ljava/lang/X$$Lambda;)V"
        ));
        assert!(!bound_method_type_shareable(&ctx, "not-a-descriptor"));
        for desc in ["(Lapp/Foo;)I", "(Ljava/util/NotLoadedHere;)V"] {
            let a = table_type(&mut ctx, desc);
            let b = table_type(&mut ctx, desc);
            assert_ne!(a, b, "{desc}: not shared");
        }
        assert!(ctx.bound_method_type_get("(Lapp/Foo;)I").is_none());
    }

    /// A garbage descriptor keeps the builder's `Ok(None)`.
    #[test]
    fn a_garbage_descriptor_answers_none() {
        let mut ctx = MockNativeContext::new();
        assert!(matches!(
            bound_method_type_through_table(&mut ctx, "x", None),
            Ok(None)
        ));
        assert!(ctx.bound_method_type_get("x").is_none());
    }
}

/// JIT round 16 (lane mh): the door refusals `R16MhErrors` found missing
/// against HotSpot 25 (the return conversion, a collector at another arity,
/// `bindTo(null)`).
#[cfg(test)]
mod r16_mh_door_refusal_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    fn handle(ctx: &mut MockNativeContext, desc: &str, kind: i32) -> ObjectRef {
        match alloc_method_handle(ctx, "p/C", "m", desc, kind) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc {desc}: {e:?}"),
        }
    }

    fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
        match err {
            MethodCallFailed::ExceptionThrown(o) => {
                ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                    == Some("java/lang/invoke/WrongMethodTypeException")
            }
            _ => false,
        }
    }

    /// `(String) intToInt.invoke(3)` / `(short) ...` are `asType`'s refusal;
    /// a boxing, widening, `Object` or `void` return converts.
    #[test]
    fn a_return_the_handle_cannot_convert_to_is_refused() {
        if !mh_strict_exact_signature() || !mh_invoke_return_refusal_enabled() {
            return;
        }
        let mut ctx = MockNativeContext::new();
        let m = handle(&mut ctx, "(I)I", MH_KIND_STATIC);
        for refused in [
            "(I)Ljava/lang/String;",
            "(I)S",
            "(I)Ljava/lang/Long;",
            "(I)Z",
        ] {
            match invoke_narrowing_arg_refusal(&mut ctx, m, MH_KIND_STATIC, refused, Some("(I)I")) {
                Some(e) => assert!(is_wmte(&ctx, &e), "{refused}: {e:?}"),
                None => panic!("{refused}: an int return does not convert"),
            }
        }
        for converts in [
            "(I)Ljava/lang/Integer;",
            "(I)Ljava/lang/Number;",
            "(I)Ljava/lang/Object;",
            "(I)J",
            "(I)D",
            "(I)V",
        ] {
            assert!(
                invoke_narrowing_arg_refusal(&mut ctx, m, MH_KIND_STATIC, converts, Some("(I)I"))
                    .is_none(),
                "{converts} converts"
            );
        }
    }

    /// A collector called at another arity converts each collected argument
    /// to the component (`vsum(int...)` at `(int,String)int` is refused); an
    /// array at the site's tail is the pass-through, not an element.
    #[test]
    fn a_collector_at_another_arity_converts_each_collected_argument() {
        if !mh_strict_exact_signature() || !mh_invoke_collector_arity_refusal_enabled() {
            return;
        }
        let mut ctx = MockNativeContext::new();
        let vsum = handle(&mut ctx, "([I)I", MH_KIND_STATIC);
        mh_set_varargs_collector(&ctx, vsum, true);
        let declared = Some("([I)I");
        match invoke_narrowing_arg_refusal(
            &mut ctx,
            vsum,
            MH_KIND_STATIC,
            "(ILjava/lang/String;)I",
            declared,
        ) {
            Some(e) => assert!(is_wmte(&ctx, &e), "{e:?}"),
            None => panic!("a String is no int"),
        }
        for converts in [
            "(III)I",
            "()I",
            "(Ljava/lang/Object;I)I",
            "(ISB)I",
            "(I)Ljava/lang/Object;",
        ] {
            assert!(
                invoke_narrowing_arg_refusal(&mut ctx, vsum, MH_KIND_STATIC, converts, declared)
                    .is_none(),
                "{converts} collects"
            );
        }
        assert!(
            invoke_narrowing_arg_refusal(
                &mut ctx,
                vsum,
                MH_KIND_STATIC,
                "(II)Ljava/lang/String;",
                declared
            )
            .is_some(),
            "the return is judged at a collecting arity too"
        );
        // Not a collector: another arity is not this refusal's.
        mh_set_varargs_collector(&ctx, vsum, false);
        assert!(invoke_narrowing_arg_refusal(
            &mut ctx,
            vsum,
            MH_KIND_STATIC,
            "(ILjava/lang/String;)I",
            declared
        )
        .is_none());
        // The array pass-through at equal arity.
        let tail = handle(&mut ctx, "(Ljava/lang/String;[I)V", MH_KIND_STATIC);
        mh_set_varargs_collector(&ctx, tail, true);
        assert!(invoke_narrowing_arg_refusal(
            &mut ctx,
            tail,
            MH_KIND_STATIC,
            "(Ljava/lang/Object;[I)V",
            Some("(Ljava/lang/String;[I)V")
        )
        .is_none());
        assert_eq!(
            collected_descriptor(&["I".into(), "[J".into()], "V", "J", 2),
            "(IJJ)V"
        );
    }

    /// `bindTo(null)` takes the insert adapter for every capture kind only.
    #[test]
    fn a_null_capture_takes_the_insert_adapter() {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_BINDTO_NULL_INSERT") {
            return;
        }
        let null = Value::Object(None);
        for kind in [
            MH_KIND_STATIC,
            MH_KIND_VIRTUAL,
            MH_KIND_SPECIAL,
            MH_KIND_ARRAY_GET,
        ] {
            assert!(bindto_null_takes_insert_adapter(
                kind,
                null,
                "(Ljava/lang/String;)V"
            ));
        }
        assert!(!bindto_null_takes_insert_adapter(
            MH_KIND_CONSTANT,
            null,
            "()V"
        ));
        assert!(!bindto_null_takes_insert_adapter(
            MH_KIND_LAMBDA_FACTORY,
            null,
            "(Ljava/lang/String;)V"
        ));
        let mut ctx = MockNativeContext::new();
        let s = ctx.create_string("x");
        assert!(!bindto_null_takes_insert_adapter(
            MH_KIND_STATIC,
            Value::Object(Some(s)),
            "(Ljava/lang/String;)V"
        ));
    }
}

/// JIT round 16 (lane mh), follow-up: the converting lane (MH16-2), the
/// `MethodType` render memo (MH16-3) and the varargs collector's array type
/// (MH5-2).
#[cfg(test)]
mod r16_mh_lane_b_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
        match build_method_type_from_descriptor(ctx, desc) {
            Ok(Some(mt)) => mt,
            other => panic!("MethodType {desc}: {other:?}"),
        }
    }

    /// `MethodType`'s `rtype` / `ptypes` declared, so a `type` reads back.
    fn setup() -> MockNativeContext {
        let mut ctx = MockNativeContext::new();
        for name in [
            "p/C",
            "java/lang/Integer",
            "java/lang/String",
            "[Ljava/lang/String;",
        ] {
            let _ = ctx.ensure_class_initialized(name);
        }
        let probe = mt(&mut ctx, "()V");
        let cid = ctx.class_id_of_object(probe);
        let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            cid,
            vec![
                field("rtype", "Ljava/lang/Class;", 0),
                field("ptypes", "[Ljava/lang/Class;", 1),
            ],
        );
        ctx
    }

    fn handle(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
        match alloc_method_handle(ctx, "p/C", "m", desc, MH_KIND_STATIC) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc {desc}: {e:?}"),
        }
    }

    fn object(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
        let o = match ctx.new_object(class) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("new_object {class}: {other:?}"),
        };
        ctx.set_field(o, 0, payload);
        Value::Object(Some(o))
    }

    fn stamp(ctx: &mut MockNativeContext, h: ObjectRef, desc: &str) -> ObjectRef {
        let t = mt(ctx, desc);
        match mh_with_stamped_type(ctx, Value::Object(Some(h)), t, false) {
            Ok(Some(Value::Object(Some(copy)))) => copy,
            other => panic!("stamp {desc}: {other:?}"),
        }
    }

    /// A result as the test compares it: the class of a thrown or returned
    /// object, else the value.
    fn describe(ctx: &MockNativeContext, r: &MethodCallResult) -> String {
        match r {
            Ok(Some(Value::Object(Some(o)))) => {
                format!("obj {:?}", ctx.class_name_of_id(ctx.class_id_of_object(*o)))
            }
            Ok(v) => format!("{v:?}"),
            Err(MethodCallFailed::ExceptionThrown(o)) => {
                format!(
                    "throw {:?}",
                    ctx.class_name_of_id(ctx.class_id_of_object(*o))
                )
            }
            Err(e) => format!("{e:?}"),
        }
    }

    /// The converting lane answers exactly what the door answers, through both
    /// doors, for the shapes it admits, and releases every pin.
    #[test]
    fn the_converting_lane_answers_as_the_door() {
        let mut ctx = setup();
        let h = handle(&mut ctx, "(I)I");
        let obj_obj = "(Ljava/lang/Object;)Ljava/lang/Object;";
        let copy = stamp(&mut ctx, h, obj_obj);
        let int5 = object(&mut ctx, "java/lang/Integer", Value::Int(5));
        let s = ctx.create_string("x");
        let cases: Vec<(ObjectRef, Value, &str, DirectLaneDoor)> = vec![
            (copy, int5, obj_obj, DirectLaneDoor::InvokeExact),
            (
                copy,
                Value::Object(Some(s)),
                obj_obj,
                DirectLaneDoor::InvokeExact,
            ),
            (
                copy,
                Value::Object(None),
                obj_obj,
                DirectLaneDoor::InvokeExact,
            ),
            (h, int5, obj_obj, DirectLaneDoor::Invoke),
            (h, Value::Object(Some(s)), obj_obj, DirectLaneDoor::Invoke),
            (h, Value::Long(5), "(J)I", DirectLaneDoor::Invoke),
            (h, Value::Int(5), "(I)J", DirectLaneDoor::Invoke),
        ];
        for (mh, arg, site, door) in cases {
            let lane = mh_converting_lane(&mut ctx, mh, &[arg], Some(site), door);
            let Some(lane) = lane else {
                panic!("{site} {door:?}: the lane declined");
            };
            let ordinary = match door {
                DirectLaneDoor::InvokeExact => {
                    mh_invoke_exact_door(&mut ctx, mh, &[arg], Some(site), true)
                }
                DirectLaneDoor::Invoke => mh_invoke_door(&mut ctx, mh, &[arg], Some(site), true),
            };
            assert_eq!(
                describe(&ctx, &lane),
                describe(&ctx, &ordinary),
                "{site} {door:?} {arg:?}"
            );
        }
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    /// Everything outside the converting lane's shape is declined before it
    /// has done anything.
    #[test]
    fn the_converting_lane_declines_what_it_does_not_model() {
        let mut ctx = setup();
        let obj_obj = "(Ljava/lang/Object;)Ljava/lang/Object;";
        let h = handle(&mut ctx, "(I)I");
        let copy = stamp(&mut ctx, h, obj_obj);
        let int5 = object(&mut ctx, "java/lang/Integer", Value::Int(5));
        let exact = DirectLaneDoor::InvokeExact;
        // `invokeExact` at a site that is not the declared type.
        assert!(mh_converting_lane(&mut ctx, copy, &[int5], Some("(I)I"), exact).is_none());
        // No call site; another arity.
        assert!(mh_converting_lane(&mut ctx, copy, &[int5], None, exact).is_none());
        assert!(
            mh_converting_lane(&mut ctx, copy, &[], Some("()Ljava/lang/Object;"), exact).is_none()
        );
        // A collector, an explicit stamp, a bound handle, a virtual handle.
        let collector = stamp(&mut ctx, h, obj_obj);
        mh_set_varargs_collector(&ctx, collector, true);
        assert!(mh_converting_lane(&mut ctx, collector, &[int5], Some(obj_obj), exact).is_none());
        let explicit = stamp(&mut ctx, h, obj_obj);
        mh_mark_explicit_cast(&ctx, explicit);
        assert!(mh_converting_lane(&mut ctx, explicit, &[int5], Some(obj_obj), exact).is_none());
        let bound = handle(&mut ctx, "(Ljava/lang/Object;)I");
        ctx.set_field(bound, MH_BOUND, int5);
        assert!(mh_converting_lane(&mut ctx, bound, &[], Some("()I"), exact).is_none());
        let virt = match alloc_method_handle(&mut ctx, "p/C", "m", "(I)I", MH_KIND_VIRTUAL) {
            Ok(v) => v,
            Err(e) => panic!("alloc virtual: {e:?}"),
        };
        assert!(mh_converting_lane(
            &mut ctx,
            virt,
            &[int5, Value::Int(1)],
            Some("(Lp/C;I)I"),
            DirectLaneDoor::Invoke
        )
        .is_none());
        // An array parameter anywhere.
        let arr = handle(&mut ctx, "([I)I");
        let empty = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 0)));
        assert!(mh_converting_lane(&mut ctx, arr, &[empty], Some("([I)I"), exact).is_none());
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "nothing pinned by a decline"
        );
    }

    /// MH16-3: a second render of one `MethodType` in one epoch is the memo's
    /// answer (shown by rewriting `ptypes` behind it, which the invariant
    /// forbids in production), the plain render is not memoised, and a
    /// context with VM identity `0` is never memoised.
    #[test]
    fn the_type_render_memo_answers_a_second_render() {
        let mut ctx = setup();
        ctx.set_vm_identity(0x16A1_0001);
        let t = mt(&mut ctx, "(I)I");
        assert_eq!(
            methodtype_to_descriptor_memoised(&mut ctx, t).as_deref(),
            Some("(I)I")
        );
        let long = ctx.primitive_class_mirror("long");
        let Value::Object(Some(ptypes)) = ctx.get_field_by_name(t, "ptypes") else {
            panic!("no ptypes");
        };
        ctx.set_array_element(ptypes, 0, Value::Object(Some(long)));
        assert_eq!(
            methodtype_to_descriptor_memoised(&mut ctx, t).as_deref(),
            Some("(I)I")
        );
        assert_eq!(
            methodtype_to_descriptor_rendered(&mut ctx, t).as_deref(),
            Some("(J)I")
        );
        let mut plain = setup();
        let u = mt(&mut plain, "(I)I");
        assert_eq!(
            methodtype_to_descriptor_memoised(&mut plain, u).as_deref(),
            Some("(I)I")
        );
        let Value::Object(Some(ptypes)) = plain.get_field_by_name(u, "ptypes") else {
            panic!("no ptypes");
        };
        let long = plain.primitive_class_mirror("long");
        plain.set_array_element(ptypes, 0, Value::Object(Some(long)));
        assert_eq!(
            methodtype_to_descriptor_memoised(&mut plain, u).as_deref(),
            Some("(J)I")
        );
    }

    /// MH5-2: `asVarargsCollector(String[].class)` over `(Object,Object)Object`
    /// answers a collector of type `(Object,String[])Object` (the receiver
    /// untouched), and its entry collects against `String[]`.
    #[test]
    fn a_varargs_collector_takes_its_array_type() {
        if !mh_varargs_array_type_enabled() {
            return;
        }
        let mut ctx = setup();
        let two = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
        let collected = "(Ljava/lang/Object;[Ljava/lang/String;)Ljava/lang/Object;";
        let h = handle(&mut ctx, two);
        let answer =
            mh_as_varargs_collector_retyped(&mut ctx, Some(Value::Object(Some(h))), collected);
        let c = match answer {
            Some(Ok(Some(Value::Object(Some(c))))) => c,
            other => panic!("retyped: {other:?}"),
        };
        assert_ne!(c, h, "a copy");
        assert!(mh_is_varargs_collector(&ctx, c));
        assert!(!mh_is_varargs_collector(&ctx, h));
        assert_eq!(
            mh_declared_descriptor(&mut ctx, c).as_deref(),
            Some(collected)
        );
        assert_eq!(mh_declared_descriptor(&mut ctx, h).as_deref(), Some(two));
        assert_eq!(
            mh_collector_declared_array_desc(&mut ctx, c, two).as_deref(),
            Some(collected)
        );
        assert!(mh_collector_declared_array_desc(&mut ctx, h, two).is_none());
        // A primitive trailing leaf parameter is never retyped at entry.
        let prim_tail = "(Ljava/lang/Object;I)Ljava/lang/Object;";
        assert!(mh_collector_declared_array_desc(&mut ctx, c, prim_tail).is_none());
    }
}

/// JIT round 17 wave 2 (lane mh2): the names the `invokeExact` loader check
/// ([`mh_exact_loader_refusal`]) asks about.
#[cfg(test)]
mod r17w2_mh_exact_loader_tests {
    use super::*;

    fn names(desc: &str) -> Vec<String> {
        let mut out = Vec::new();
        mh_descriptor_any_loader_name(desc, |n| {
            out.push(n.to_string());
            false
        });
        out
    }

    #[test]
    fn the_loader_names_skip_primitives_arrays_and_java() {
        assert!(names("(IJ)I").is_empty());
        assert!(names("(Ljava/lang/String;)Ljava/lang/Object;").is_empty());
        assert_eq!(names("(LBox;[Lp/Lib;I)Lq/R;"), ["Box", "p/Lib", "q/R"]);
        assert!(mh_descriptor_any_loader_name("(ILBox;)V", |n| n == "Box"));
        assert!(!mh_descriptor_any_loader_name("(ILBox;)V", |n| n == "Other"));
    }

    /// JIT round 17 wave 2 (lane mh3): the `invoke` loader cast is a
    /// correctness fix with a `0` kill switch.
    #[test]
    fn the_invoke_loader_cast_is_default_on() {
        const SWITCH: &str = "CRATONVM_MH_INVOKE_LOADER_CAST";
        let on = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, None)], || {
            mh_invoke_loader_cast_enabled()
        });
        assert!(on, "default ON");
        let off = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("0"))], || {
            mh_invoke_loader_cast_enabled()
        });
        assert!(!off, "`0` turns it off");
    }
}

/// JIT round 16 (lane mh), wave 3: the admission of a constant handle to a
/// compiled site's direct call (MH15-5, [`mh_compiled_bind_target`]).
#[cfg(test)]
mod r16_mh_compiled_bind_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, MethodMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
        match build_method_type_from_descriptor(ctx, desc) {
            Ok(Some(mt)) => mt,
            other => panic!("MethodType {desc}: {other:?}"),
        }
    }

    fn method(cid: ClassId, name: &str, desc: &str, flags: u16) -> MethodMetadata {
        MethodMetadata {
            name: name.to_string(),
            descriptor: desc.to_string(),
            access_flags: flags,
            declaring_class_id: cid,
            exceptions: Vec::new(),
            signature: None,
        }
    }

    /// `p/C` with `static m(I)I`, `v(I)I`, private `p(I)I`, `s(Lp/I;)I`
    /// (an interface parameter) and `t(Lp/C;)I`; `p/I` an interface.
    fn setup() -> (MockNativeContext, ClassId) {
        let mut ctx = MockNativeContext::new();
        let mut cid = ClassId::new(0);
        for name in ["p/C", "p/I", "java/lang/String"] {
            let id = ctx.ensure_class_initialized(name).unwrap();
            if name == "p/C" {
                cid = id;
            }
            if name == "p/I" {
                ctx.set_is_interface(id, true);
            }
        }
        ctx.set_declared_methods(
            cid,
            vec![
                method(cid, "m", "(I)I", 0x0009),
                method(cid, "v", "(I)I", 0x0001),
                method(cid, "p", "(I)I", 0x0002),
                method(cid, "s", "(Lp/I;)I", 0x0009),
                method(cid, "t", "(Lp/C;)I", 0x0009),
            ],
        );
        let probe = mt(&mut ctx, "()V");
        let mt_cid = ctx.class_id_of_object(probe);
        let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: mt_cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            mt_cid,
            vec![
                field("rtype", "Ljava/lang/Class;", 0),
                field("ptypes", "[Ljava/lang/Class;", 1),
            ],
        );
        (ctx, cid)
    }

    fn handle(ctx: &mut MockNativeContext, name: &str, desc: &str, kind: i32) -> ObjectRef {
        match alloc_method_handle(ctx, "p/C", name, desc, kind) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc {name}{desc}: {e:?}"),
        }
    }

    #[test]
    fn a_direct_handle_at_its_own_type_binds_as_its_member() {
        let (mut ctx, cid) = setup();
        let st = handle(&mut ctx, "m", "(I)I", MH_KIND_STATIC);
        assert_eq!(
            mh_compiled_bind_target(&mut ctx, st, "(I)I"),
            Some(MhCompiledBind {
                invoke_kind: 3,
                class_id: cid.as_u32(),
                class: "p/C".into(),
                name: "m".into(),
                desc: "(I)I".into(),
                bound: false,
            })
        );
        let vi = handle(&mut ctx, "v", "(I)I", MH_KIND_VIRTUAL);
        assert_eq!(
            mh_compiled_bind_target(&mut ctx, vi, "(Lp/C;I)I").map(|b| b.invoke_kind),
            Some(0)
        );
        let ref_param = handle(&mut ctx, "t", "(Lp/C;)I", MH_KIND_STATIC);
        assert!(mh_compiled_bind_target(&mut ctx, ref_param, "(Lp/C;)I").is_some());
    }

    #[test]
    fn every_shape_the_door_could_refuse_or_reshape_is_refused() {
        let (mut ctx, _) = setup();
        let st = handle(&mut ctx, "m", "(I)I", MH_KIND_STATIC);
        // Another site type; a marking bit; a bound value.
        assert!(mh_compiled_bind_target(&mut ctx, st, "(J)I").is_none());
        let collector = handle(&mut ctx, "m", "(I)I", MH_KIND_STATIC);
        mh_set_varargs_collector(&ctx, collector, true);
        assert!(mh_compiled_bind_target(&mut ctx, collector, "(I)I").is_none());
        let vi = handle(&mut ctx, "v", "(I)I", MH_KIND_VIRTUAL);
        let s = ctx.create_string("x");
        ctx.set_field(vi, MH_BOUND, Value::Object(Some(s)));
        assert!(mh_compiled_bind_target(&mut ctx, vi, "(I)I").is_none());
        // An interface parameter (the verifier does not type it).
        let iface = handle(&mut ctx, "s", "(Lp/I;)I", MH_KIND_STATIC);
        assert!(mh_compiled_bind_target(&mut ctx, iface, "(Lp/I;)I").is_none());
        // A private virtual target; a kind that is not the member's.
        let private = handle(&mut ctx, "p", "(I)I", MH_KIND_VIRTUAL);
        assert!(mh_compiled_bind_target(&mut ctx, private, "(Lp/C;I)I").is_none());
        let not_static = handle(&mut ctx, "v", "(I)I", MH_KIND_STATIC);
        assert!(mh_compiled_bind_target(&mut ctx, not_static, "(I)I").is_none());
        // A member the class does not declare.
        let missing = handle(&mut ctx, "q", "(I)I", MH_KIND_STATIC);
        assert!(mh_compiled_bind_target(&mut ctx, missing, "(I)I").is_none());
        // A JDK member (caller-sensitive methods see their caller).
        let jdk = match alloc_method_handle(
            &mut ctx,
            "java/lang/String",
            "valueOf",
            "(I)Ljava/lang/String;",
            MH_KIND_STATIC,
        ) {
            Ok(h) => h,
            Err(e) => panic!("alloc jdk: {e:?}"),
        };
        assert!(mh_compiled_bind_target(&mut ctx, jdk, "(I)Ljava/lang/String;").is_none());
        // An array parameter.
        let arr = handle(&mut ctx, "m", "([I)I", MH_KIND_STATIC);
        assert!(mh_compiled_bind_target(&mut ctx, arr, "([I)I").is_none());
        // The guard slots are the decided-on ones, each below the minted width.
        assert!(MH_COMPILED_BIND_GUARD_SLOTS
            .iter()
            .all(|s| *s <= MH_STATIC_OWNER));
    }

    /// A `bindTo` of `v(I)I` as `bindTo` mints it: `MH_BOUND` the receiver,
    /// `type` the member's own descriptor.
    fn bound_virtual(ctx: &mut MockNativeContext, receiver: ObjectRef) -> ObjectRef {
        let h = handle(ctx, "v", "(I)I", MH_KIND_VIRTUAL);
        ctx.set_field(h, MH_BOUND, Value::Object(Some(receiver)));
        let t = mt(ctx, "(I)I");
        ctx.set_field_by_name(h, "type", Value::Object(Some(t)));
        h
    }

    /// Round 16 wave 8 (lane mh): a bound virtual handle binds as its member,
    /// called on the bound receiver, only with the switch's parameter on, and
    /// only at its bound type.
    #[test]
    fn a_bound_virtual_handle_binds_as_its_member_on_its_receiver() {
        let (mut ctx, cid) = setup();
        let receiver = ctx.alloc_object(cid, 4);
        let bound = bound_virtual(&mut ctx, receiver);
        assert!(mh_compiled_bind_target_with(&mut ctx, bound, "(I)I", false).is_none());
        assert_eq!(
            mh_compiled_bind_target_with(&mut ctx, bound, "(I)I", true),
            Some(MhCompiledBind {
                invoke_kind: 0,
                class_id: cid.as_u32(),
                class: "p/C".into(),
                name: "v".into(),
                desc: "(I)I".into(),
                bound: true,
            })
        );
        // The receiver is no call-site argument.
        assert!(mh_compiled_bind_target_with(&mut ctx, bound, "(Lp/C;I)I", true).is_none());
        // A receiver of a subclass binds too (`invokevirtual` selects on it).
        let sub = ctx.ensure_class_initialized("p/D").unwrap();
        ctx.set_superclass(sub, cid);
        let sub_receiver = ctx.alloc_object(sub, 4);
        let bound_sub = bound_virtual(&mut ctx, sub_receiver);
        assert!(
            mh_compiled_bind_target_with(&mut ctx, bound_sub, "(I)I", true)
                .is_some_and(|b| b.bound)
        );
        // The helper reads the receiver from the `MH_BOUND` guard word.
        assert_eq!(
            MH_COMPILED_BIND_GUARD_SLOTS[MH_COMPILED_BIND_BOUND_GUARD],
            MH_BOUND
        );
    }

    /// Round 16 wave 8 (lane mh): what the bound admission refuses -- a bound
    /// value the member's class does not admit (`bindTo` would have refused
    /// it; the door's `leaf_receiver_refusal` would), an array, a bound
    /// STATIC handle (its leading argument is the door's to place), and a
    /// `type` that is not the member's own descriptor.
    #[test]
    fn a_bound_receiver_the_member_does_not_take_is_refused() {
        let (mut ctx, cid) = setup();
        let s = ctx.create_string("x");
        let wrong = bound_virtual(&mut ctx, s);
        assert!(mh_compiled_bind_target_with(&mut ctx, wrong, "(I)I", true).is_none());
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 1);
        let array_bound = bound_virtual(&mut ctx, arr);
        assert!(mh_compiled_bind_target_with(&mut ctx, array_bound, "(I)I", true).is_none());
        let receiver = ctx.alloc_object(cid, 4);
        let st = handle(&mut ctx, "t", "(Lp/C;)I", MH_KIND_STATIC);
        ctx.set_field(st, MH_BOUND, Value::Object(Some(receiver)));
        let t = mt(&mut ctx, "()I");
        ctx.set_field_by_name(st, "type", Value::Object(Some(t)));
        assert!(mh_compiled_bind_target_with(&mut ctx, st, "()I", true).is_none());
        let stale_type = handle(&mut ctx, "v", "(I)I", MH_KIND_VIRTUAL);
        ctx.set_field(stale_type, MH_BOUND, Value::Object(Some(receiver)));
        // `type` left the unbound `(Lp/C;I)I`: not the bound site's.
        assert!(mh_compiled_bind_target_with(&mut ctx, stale_type, "(I)I", true).is_none());
    }

    /// The wrapper classes the converting admission names.
    fn ensure_wrappers(ctx: &mut MockNativeContext) -> ClassId {
        for name in ["java/lang/Object", "java/lang/Number", "java/lang/Long"] {
            ctx.ensure_class_initialized(name).unwrap();
        }
        ctx.ensure_class_initialized("java/lang/Integer").unwrap()
    }

    /// `asType(t)` (or `explicitCastArguments`) of `h`, as the native answers it.
    fn stamp(ctx: &mut MockNativeContext, h: ObjectRef, desc: &str, explicit: bool) -> ObjectRef {
        let t = mt(ctx, desc);
        match mh_with_stamped_type(ctx, Value::Object(Some(h)), t, explicit) {
            Ok(Some(Value::Object(Some(copy)))) => copy,
            other => panic!("asType {desc}: {other:?}"),
        }
    }

    /// JIT round 16 wave 15 (lane mh6; MHB-1): the closed set of conversions,
    /// position by position, and nothing else.
    #[test]
    fn the_converting_bind_models_only_the_int_wrapper_conversions() {
        let obj = "Ljava/lang/Object;";
        let cases: [(&str, &str, Option<(u32, bool)>); 9] = [
            (
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                "(I)I",
                Some((1, true)),
            ),
            ("(Ljava/lang/Integer;)I", "(I)I", Some((1, false))),
            ("(I)Ljava/lang/Number;", "(I)I", Some((0, true))),
            (
                "(Lp/C;Ljava/lang/Object;)I",
                "(Lp/C;I)I",
                Some((0b10, false)),
            ),
            // Nothing converts: the exact bind's.
            ("(I)I", "(I)I", None),
            // Widening, another wrapper, a dropped return, a narrowing.
            ("(S)I", "(I)I", None),
            ("(Ljava/lang/Long;)I", "(I)I", None),
            ("(I)V", "(I)I", None),
            ("(Ljava/lang/Object;)Ljava/lang/Object;", "(J)J", None),
        ];
        for (from, to, want) in cases {
            assert_eq!(mh_bind_conversions(from, to), want, "{from} -> {to}");
        }
        assert!(mh_bind_int_wrapper_token(obj));
        assert!(!mh_bind_int_wrapper_token("Ljava/lang/String;"));
    }

    /// JIT round 16 wave 15 (lane mh6; MHB-1): `m(I)I` stamped to
    /// `(Object)Object` binds at its declared type through either door, and
    /// the unstamped handle binds an `invoke` at a converting site; an
    /// `invokeExact` there stays the door's (`WrongMethodTypeException`).
    #[test]
    fn a_one_hop_int_conversion_binds_with_its_conversions() {
        let (mut ctx, cid) = setup();
        let integer = ensure_wrappers(&mut ctx);
        let obj_obj = "(Ljava/lang/Object;)Ljava/lang/Object;";
        let st = handle(&mut ctx, "m", "(I)I", MH_KIND_STATIC);
        let copy = stamp(&mut ctx, st, obj_obj, false);
        let want = MhCompiledBind {
            invoke_kind: 3,
            class_id: cid.as_u32(),
            class: "p/C".into(),
            name: "m".into(),
            desc: "(I)I".into(),
            bound: false,
        };
        let conv = MhBindConversion {
            unbox_int: 1,
            box_int_return: true,
            integer_class_id: integer.as_u32(),
        };
        assert!(mh_compiled_bind_target(&mut ctx, copy, obj_obj).is_none());
        for invoke_door in [false, true] {
            assert_eq!(
                mh_compiled_bind_converting_target(&mut ctx, copy, obj_obj, invoke_door),
                Some((want.clone(), conv))
            );
        }
        assert_eq!(
            mh_compiled_bind_converting_target(&mut ctx, st, obj_obj, true),
            Some((want.clone(), conv))
        );
        assert!(mh_compiled_bind_converting_target(&mut ctx, st, obj_obj, false).is_none());
        // The member's own type is the exact bind's, not this one's.
        assert!(mh_compiled_bind_converting_target(&mut ctx, st, "(I)I", true).is_none());
        // An unbound virtual handle: its receiver passes through as word 0.
        let vi = handle(&mut ctx, "v", "(I)I", MH_KIND_VIRTUAL);
        assert_eq!(
            mh_compiled_bind_converting_target(
                &mut ctx,
                vi,
                "(Lp/C;Ljava/lang/Object;)Ljava/lang/Object;",
                true
            )
            .map(|(b, c)| (b.invoke_kind, c.unbox_int, c.box_int_return)),
            Some((0, 0b10, true))
        );
    }

    /// JIT round 16 wave 15 (lane mh6; MHB-1): what the converting admission
    /// leaves to the door -- two `asType` hops (a copy of a copy, `invoke` on
    /// a copy at another type), an `explicitCastArguments` copy, a bound
    /// handle, an interface parameter, and a receiver the site retypes.
    #[test]
    fn a_conversion_past_one_hop_or_off_the_exact_rules_is_refused() {
        let (mut ctx, cid) = setup();
        ensure_wrappers(&mut ctx);
        let obj_obj = "(Ljava/lang/Object;)Ljava/lang/Object;";
        let st = handle(&mut ctx, "m", "(I)I", MH_KIND_STATIC);
        let c1 = stamp(&mut ctx, st, "(Ljava/lang/Integer;)I", false);
        let c2 = stamp(&mut ctx, c1, "(Ljava/lang/Object;)I", false);
        assert!(
            mh_compiled_bind_converting_target(&mut ctx, c2, "(Ljava/lang/Object;)I", false)
                .is_none()
        );
        assert!(
            mh_compiled_bind_converting_target(&mut ctx, c1, "(Ljava/lang/Object;)I", true)
                .is_none()
        );
        let explicit = stamp(&mut ctx, st, obj_obj, true);
        assert!(mh_compiled_bind_converting_target(&mut ctx, explicit, obj_obj, false).is_none());
        let receiver = ctx.alloc_object(cid, 4);
        let bound = bound_virtual(&mut ctx, receiver);
        assert!(mh_compiled_bind_converting_target(&mut ctx, bound, obj_obj, true).is_none());
        let iface = handle(&mut ctx, "s", "(Lp/I;)I", MH_KIND_STATIC);
        assert!(mh_compiled_bind_converting_target(
            &mut ctx,
            iface,
            "(Lp/I;)Ljava/lang/Object;",
            true
        )
        .is_none());
        let vi = handle(&mut ctx, "v", "(I)I", MH_KIND_VIRTUAL);
        assert!(mh_compiled_bind_converting_target(
            &mut ctx,
            vi,
            "(Ljava/lang/Object;Ljava/lang/Object;)I",
            true
        )
        .is_none());
    }
}

/// JIT round 16 wave 8 (lane mh; proposal MH5-1 of
/// `jit-r15w6-mh5-proposals-RETIRED-20261003.md`): door natives under the moving mock heap
/// (`MockNativeContext::set_moving_heap`). Every allocation and every Java call
/// relocates every object and retires the old addresses, so a native that
/// keeps a reference across one without a pin fails here ("stale ObjectRef")
/// instead of passing, as it did with the never-moving mock. The test's own
/// locals are not roots: each test re-reads what it holds through a global
/// root, and reads results through what the native returned. Each context has
/// a private VM identity (the `valueOf` caches and the address-keyed memos are
/// per VM); boxed values sit outside the `Integer` cache (a process-wide
/// per-VM table this mock does not forward).
#[cfg(test)]
mod r16w8_mh_moving_heap_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    const ONE: &str = "(Ljava/lang/String;)Ljava/lang/String;";

    fn moving_ctx(identity: usize) -> MockNativeContext {
        let ctx = MockNativeContext::new();
        ctx.set_vm_identity(identity);
        ctx
    }

    fn root(ctx: &MockNativeContext, handle: usize) -> ObjectRef {
        match ctx.resolve_global_root(handle) {
            Some(o) => o,
            None => panic!("global root {handle} is gone"),
        }
    }

    /// `alloc_resolved_member_name` (`InvokerBytecodeGenerator`'s customized-
    /// code / entry-point bypasses) keeps its new `MemberName` rooted across
    /// the mirror, name-string and `MethodType` allocations. As a raw local,
    /// a G1 pause between them sent the vmindex sentinel into the real
    /// object's `method` slot and the later stores (`type`, `flags`, a null
    /// `resolution`) into whatever object took the old address
    /// (`ObjectStreamLookupGcRaceProbe`'s early stream NPE). Under the moving
    /// mock every allocation relocates the heap, so a stale `mn` panics.
    #[test]
    fn a_resolved_member_name_survives_a_relocation_per_allocation() {
        let mut ctx = moving_ctx(0x16A8_0002);
        ctx.set_moving_heap(true);
        let before = ctx.relocations_for_test();
        let mn = alloc_resolved_member_name(
            &mut ctx,
            "java/lang/invoke/LambdaForm",
            "interpret_V",
            "()V",
        )
        .expect("a MemberName is built");
        assert!(
            ctx.relocations_for_test() >= before + 3,
            "the mirror, the name and the MethodType each moved the heap"
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
        match mn_get(&mut ctx, mn, "name", MN_NAME) {
            Value::Object(Some(s)) => {
                assert_eq!(ctx.read_string(s).as_deref(), Some("interpret_V"))
            }
            other => panic!("name: {other:?}"),
        }
        assert!(matches!(
            mn_get(&mut ctx, mn, "type", MN_TYPE),
            Value::Object(Some(_))
        ));
        assert_ne!(mn_get_vmindex(&mut ctx, mn), 0);
        assert!(matches!(
            mn_get(&mut ctx, mn, "resolution", MN_RESOLUTION),
            Value::Object(None)
        ));
    }

    /// `build_varargs_array` into an `Object[]`: the array and both strings
    /// are held across the boxing allocation (round 15 wave 6's GC-safety fix,
    /// found by reading); under the moving mock each must be read back through
    /// its pin, or this test fails.
    #[test]
    fn varargs_collection_survives_a_relocation_per_box() {
        let mut ctx = moving_ctx(0x16A8_0001);
        let a = Value::Object(Some(ctx.create_string("a")));
        let b = Value::Object(Some(ctx.create_string("b")));
        ctx.set_moving_heap(true);
        let before = ctx.relocations_for_test();
        let arr = build_varargs_array(
            &mut ctx,
            "Ljava/lang/Object;",
            &[a, Value::Int(1000), b],
            None,
            0,
        )
        .expect("an Object[] is built");
        assert!(
            ctx.relocations_for_test() >= before + 2,
            "the array and the box moved the heap"
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
        let element = |ctx: &MockNativeContext, i: usize| match ctx.get_array_element(arr, i) {
            Value::Object(Some(o)) => o,
            other => panic!("element {i}: {other:?}"),
        };
        assert_eq!(ctx.read_string(element(&ctx, 0)).as_deref(), Some("a"));
        assert_eq!(
            crate::lang_class::unbox_value(&ctx, element(&ctx, 1)),
            Value::Int(1000)
        );
        assert_eq!(ctx.read_string(element(&ctx, 2)).as_deref(), Some("b"));
    }

    /// `bindTo` of a virtual handle allocates the bound handle and its `type`
    /// with the target and the receiver held: both are read back through
    /// their pins (gen r4w3/rooting).
    #[test]
    fn bind_to_keeps_its_receiver_across_its_allocations() {
        let mut ctx = moving_ctx(0x16A8_0002);
        let concat =
            match alloc_method_handle(&mut ctx, "java/lang/String", "concat", ONE, MH_KIND_VIRTUAL)
            {
                Ok(mh) => mh,
                Err(e) => panic!("alloc: {e:?}"),
            };
        ctx.set_field_by_name(concat, "type", Value::Object(None));
        let dash = ctx.create_string("-");
        let mut registry = NativeMethodRegistry::new();
        register_t4_method_handle_invoke(&mut registry);
        let bind = registry
            .find(
                "java/lang/invoke/MethodHandle",
                "bindTo",
                "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
            )
            .expect("MethodHandle.bindTo should be registered");
        ctx.set_moving_heap(true);
        let before = ctx.relocations_for_test();
        let bound = match bind(
            &mut ctx,
            &[Value::Object(Some(concat)), Value::Object(Some(dash))],
        ) {
            Ok(Some(Value::Object(Some(h)))) => h,
            other => panic!("bindTo answered {other:?}"),
        };
        assert!(ctx.relocations_for_test() > before, "bindTo allocated");
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
        assert_eq!(ctx.get_field(bound, MH_KIND), Value::Int(MH_KIND_VIRTUAL));
        match ctx.get_field(bound, MH_BOUND) {
            Value::Object(Some(r)) => assert_eq!(ctx.read_string(r).as_deref(), Some("-")),
            other => panic!("no bound receiver: {other:?}"),
        }
        assert_eq!(mh_read_name(&ctx, bound).as_deref(), Some("concat"));
    }

    /// `m(I)I` answers its argument plus 100, after checking that the
    /// receiver it was handed is current (the mock forwards a Java call's
    /// receiver as the VM's frame does).
    fn plus_hundred(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        name: &str,
        desc: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        let _ = ctx.class_id_of_object(receiver);
        match (name, desc, args.first()) {
            ("m", "(I)I", Some(Value::Int(x))) => Some(Ok(Some(Value::Int(x + 100)))),
            _ => None,
        }
    }

    fn answered_int(ctx: &MockNativeContext, r: &Option<MethodCallResult>) -> Option<i32> {
        match r {
            Some(Ok(Some(Value::Int(v)))) => Some(*v),
            Some(Ok(Some(Value::Object(Some(o))))) => match crate::lang_class::unbox_value(ctx, *o)
            {
                Value::Int(v) => Some(v),
                _ => None,
            },
            _ => None,
        }
    }

    /// `MethodType`'s `rtype` / `ptypes` declared (as in
    /// `r13w10_callcost4_direct_lane_tests`), so a handle's `type` reads back
    /// and the direct lane engages.
    fn lane_ctx(identity: usize) -> MockNativeContext {
        let mut ctx = moving_ctx(identity);
        let _ = ctx.ensure_class_initialized("p/Mid");
        let probe = match build_method_type_from_descriptor(&mut ctx, "()V") {
            Ok(Some(mt)) => mt,
            other => panic!("MethodType: {other:?}"),
        };
        let cid = ctx.class_id_of_object(probe);
        let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            cid,
            vec![
                field("rtype", "Ljava/lang/Class;", 0),
                field("ptypes", "[Ljava/lang/Class;", 1),
            ],
        );
        ctx
    }

    /// The direct lane's virtual arm, through both doors, unbound and bound:
    /// the leaf call relocates the heap, and what the lane reads after it (the
    /// receiver for the serialization-hook rule, the answer) must be current.
    #[test]
    fn the_direct_lane_answers_across_a_relocating_leaf_call() {
        let mut ctx = lane_ctx(0x16A8_0003);
        let mh = match alloc_method_handle(&mut ctx, "p/Mid", "m", "(I)I", MH_KIND_VIRTUAL) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc: {e:?}"),
        };
        let recv = match ctx.new_object("p/Mid") {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("new_object: {other:?}"),
        };
        // `bindTo`'s shape: the receiver in `MH_BOUND`, `type` without it.
        let bound = match alloc_method_handle(&mut ctx, "p/Mid", "m", "(I)I", MH_KIND_VIRTUAL) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc: {e:?}"),
        };
        ctx.set_field(bound, MH_BOUND, Value::Object(Some(recv)));
        let dropped = match build_method_type_from_descriptor(&mut ctx, "(I)I") {
            Ok(Some(mt)) => mt,
            other => panic!("MethodType: {other:?}"),
        };
        ctx.set_field_by_name(bound, "type", Value::Object(Some(dropped)));
        let mh_root = ctx.add_global_root(mh);
        let recv_root = ctx.add_global_root(recv);
        let bound_root = ctx.add_global_root(bound);
        ctx.set_invoke_virtual_hook(plus_hundred);
        ctx.set_moving_heap(true);
        for door in [DirectLaneDoor::InvokeExact, DirectLaneDoor::Invoke] {
            let before = ctx.relocations_for_test();
            let (mh, recv) = (root(&ctx, mh_root), root(&ctx, recv_root));
            let r = mh_direct_lane(
                &mut ctx,
                mh,
                &[Value::Object(Some(recv)), Value::Int(5)],
                Some("(Lp/Mid;I)I"),
                door,
            );
            assert_eq!(answered_int(&ctx, &r), Some(105), "{door:?}: {r:?}");
            assert!(
                ctx.relocations_for_test() > before,
                "{door:?}: the leaf call moved"
            );
            let bound = root(&ctx, bound_root);
            let r = mh_direct_lane(&mut ctx, bound, &[Value::Int(7)], Some("(I)I"), door);
            assert_eq!(answered_int(&ctx, &r), Some(107), "bound {door:?}: {r:?}");
        }
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }
}

/// JIT round 16 wave 11 (lane mhbind; proposal MHF-2): the `bindTo` fast mint.
/// The memo is forced on for the test's own thread (`BINDTO_MINT_TEST_VM`),
/// so the mock context (VM identity `0`, switch off) exercises it.
#[cfg(test)]
mod r16w11_mhbind_fast_mint_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, MethodMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Forces the fast mint on for this thread; off again when dropped.
    struct ForcedOn;
    impl ForcedOn {
        fn new() -> Self {
            BINDTO_MINT_TEST_VM.with(|v| v.set(Some(0x16B1_0001)));
            BINDTO_MINT_TEST_HITS.with(|h| h.set(0));
            BINDTO_MINT_MEMO.with(|m| m.borrow_mut().iter_mut().for_each(|l| *l = None));
            ForcedOn
        }
    }
    impl Drop for ForcedOn {
        fn drop(&mut self) {
            BINDTO_MINT_TEST_VM.with(|v| v.set(None));
            BINDTO_MINT_MEMO.with(|m| m.borrow_mut().iter_mut().for_each(|l| *l = None));
        }
    }

    fn hits() -> u64 {
        BINDTO_MINT_TEST_HITS.with(|h| h.get())
    }

    fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
        match build_method_type_from_descriptor(ctx, desc) {
            Ok(Some(mt)) => mt,
            other => panic!("MethodType {desc}: {other:?}"),
        }
    }

    /// `p/C` (with `v(I)I`) and its subclass `p/D`, and `MethodType`'s
    /// `rtype` / `ptypes` declared so a handle's `type` renders.
    fn setup() -> (MockNativeContext, ClassId, ClassId) {
        let mut ctx = MockNativeContext::new();
        let cid = ctx.ensure_class_initialized("p/C").unwrap();
        let sub = ctx.ensure_class_initialized("p/D").unwrap();
        ctx.set_superclass(sub, cid);
        let _ = ctx.ensure_class_initialized("java/lang/String");
        ctx.set_declared_methods(
            cid,
            vec![MethodMetadata {
                name: "v".to_string(),
                descriptor: "(I)I".to_string(),
                access_flags: 0x0001,
                declaring_class_id: cid,
                exceptions: Vec::new(),
                signature: None,
            }],
        );
        let probe = mt(&mut ctx, "()V");
        let mt_cid = ctx.class_id_of_object(probe);
        let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index,
            declaring_class_id: mt_cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            mt_cid,
            vec![
                field("rtype", "Ljava/lang/Class;", 0),
                field("ptypes", "[Ljava/lang/Class;", 1),
            ],
        );
        (ctx, cid, sub)
    }

    fn virtual_v(ctx: &mut MockNativeContext) -> ObjectRef {
        match alloc_method_handle(ctx, "p/C", "v", "(I)I", MH_KIND_VIRTUAL) {
            Ok(mh) => mh,
            Err(e) => panic!("alloc v: {e:?}"),
        }
    }

    fn bind(ctx: &mut MockNativeContext, target: ObjectRef, value: Value) -> MethodCallResult {
        let mut registry = NativeMethodRegistry::new();
        register_t4_method_handle_invoke(&mut registry);
        let bind = registry
            .find(
                "java/lang/invoke/MethodHandle",
                "bindTo",
                "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
            )
            .expect("MethodHandle.bindTo should be registered");
        bind(ctx, &[Value::Object(Some(target)), value])
    }

    fn bound(ctx: &mut MockNativeContext, target: ObjectRef, value: Value) -> ObjectRef {
        match bind(ctx, target, value) {
            Ok(Some(Value::Object(Some(h)))) => h,
            other => panic!("bindTo answered {other:?}"),
        }
    }

    /// Every slot a bound handle is read by, as words, plus its rendered `type`.
    fn shape(ctx: &mut MockNativeContext, h: ObjectRef) -> (Vec<u64>, Option<String>) {
        let words = [
            MH_CLASS,
            MH_NAME,
            MH_DESC,
            MH_KIND,
            MH_BOUND,
            MH_VARARGS,
            MH_STATIC_SETTLED,
            MH_SPECIAL_CALLER,
            MH_CAST_CHAIN,
            MH_STATIC_OWNER,
        ]
        .iter()
        .map(|s| bindto_mint_word(ctx.get_field(h, *s)))
        .collect();
        (words, mh_type_descriptor(ctx, h))
    }

    /// The second bind of one source is a hit, and the handle it mints is the
    /// slow path's, slot for slot (the slow path asked again with the memo
    /// off), with its own bound value.
    #[test]
    fn a_repeat_bind_is_a_hit_and_mints_the_slow_paths_handle() {
        let (mut ctx, cid, _) = setup();
        let src = virtual_v(&mut ctx);
        let r1 = ctx.alloc_object(cid, 4);
        let r2 = ctx.alloc_object(cid, 4);
        let (fast, first) = {
            let _on = ForcedOn::new();
            let first = bound(&mut ctx, src, Value::Object(Some(r1)));
            assert_eq!(hits(), 0, "the first bind fills");
            let fast = bound(&mut ctx, src, Value::Object(Some(r2)));
            assert_eq!(hits(), 1, "the second bind is a hit");
            (fast, first)
        };
        let slow = bound(&mut ctx, src, Value::Object(Some(r2)));
        assert_eq!(shape(&mut ctx, fast), shape(&mut ctx, slow));
        assert_eq!(mh_type_descriptor(&mut ctx, fast).as_deref(), Some("(I)I"));
        assert_eq!(ctx.get_field(fast, MH_BOUND), Value::Object(Some(r2)));
        assert_eq!(ctx.get_field(first, MH_BOUND), Value::Object(Some(r1)));
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    /// What is not a hit: a bound value of another class (a subclass the cast
    /// admits, and one it cannot judge), a null (the INSERT adapter), and a
    /// source whose marking changed in place since the line was filled.
    #[test]
    fn another_class_a_null_or_a_changed_source_is_the_slow_path() {
        let (mut ctx, cid, sub) = setup();
        let _on = ForcedOn::new();
        let src = virtual_v(&mut ctx);
        let r1 = ctx.alloc_object(cid, 4);
        let _ = bound(&mut ctx, src, Value::Object(Some(r1)));
        let d = ctx.alloc_object(sub, 4);
        let by_sub = bound(&mut ctx, src, Value::Object(Some(d)));
        assert_eq!(ctx.get_field(by_sub, MH_BOUND), Value::Object(Some(d)));
        assert_eq!(hits(), 0, "a subclass is not the remembered class");
        // A value the mock's hierarchy cannot judge (`None`): whatever the
        // slow path answers, it is not a hit and the line is not refilled.
        let s = ctx.create_string("x");
        let _ = bind(&mut ctx, src, Value::Object(Some(s)));
        assert_eq!(hits(), 0);
        let by_null = bound(&mut ctx, src, Value::Object(None));
        assert_eq!(ctx.get_field(by_null, MH_KIND), Value::Int(MH_KIND_INSERT));
        assert_eq!(hits(), 0);
        // The line now remembers `p/D` (the last fill); a `p/D` hits.
        let d2 = ctx.alloc_object(sub, 4);
        let _ = bound(&mut ctx, src, Value::Object(Some(d2)));
        assert_eq!(hits(), 1);
        // A slot rewritten in place is a miss.
        mh_set_varargs_collector(&ctx, src, true);
        let _ = bind(&mut ctx, src, Value::Object(Some(d2)));
        assert_eq!(hits(), 1, "the marking is in the key");
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    }

    /// The cast verdicts a line may remember: an `Object` leading
    /// parameter for any non-null non-array value; a positive reading by the
    /// value's exact class; nothing for "cannot tell", a null or an array.
    #[test]
    fn only_lasting_cast_verdicts_are_remembered() {
        let (mut ctx, cid, _) = setup();
        let r = ctx.alloc_object(cid, 4);
        let v = Value::Object(Some(r));
        assert_eq!(
            bindto_fast_mint_admits(&ctx, v, "Ljava/lang/Object;", None),
            Some(None)
        );
        // An array parameter is judged since wave 60: nothing is remembered.
        assert_eq!(bindto_fast_mint_admits(&ctx, v, "[I", None), None);
        assert_eq!(
            bindto_fast_mint_admits(&ctx, v, "Lp/C;", Some(true)),
            Some(Some(cid.as_u32()))
        );
        assert_eq!(bindto_fast_mint_admits(&ctx, v, "Lp/X;", None), None);
        assert_eq!(
            bindto_fast_mint_admits(&ctx, Value::Object(None), "Lp/C;", None),
            None
        );
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 1);
        assert_eq!(
            bindto_fast_mint_admits(&ctx, Value::Object(Some(arr)), "Ljava/lang/Object;", None),
            None
        );
    }

    /// MHF-1: a shape-keyed site compares every synthetic slot a `bindTo`
    /// mint writes (`MH_CLASS` through `MH_STATIC_OWNER`), each once.
    #[test]
    fn the_shape_slots_cover_every_slot_a_bound_mint_writes() {
        let mut all: Vec<usize> = MH_COMPILED_BIND_GUARD_SLOTS
            .iter()
            .chain(MH_COMPILED_BIND_SHAPE_SLOTS.iter())
            .copied()
            .collect();
        all.sort_unstable();
        let want: Vec<usize> = (MH_CLASS..=MH_STATIC_OWNER).collect();
        assert_eq!(all, want);
    }

    /// Downcall carriers (`r16-ffmcost-downcall-invoke-door-answers-null`):
    /// the slot an `asType` copy records its C descriptor in lies between this
    /// file's synthetic window and the downcall state, so an unadapted carrier
    /// (null-filled there) and an ordinary handle (narrower) record nothing.
    #[test]
    fn the_downcall_leaf_slot_sits_between_the_two_windows() {
        assert!(MH_DOWNCALL_LEAF_DESC >= MH_SLOT_COUNT);
        assert!(MH_DOWNCALL_LEAF_DESC < crate::panama::DOWNCALL_BASE);
        let (mut ctx, _, _) = setup();
        let ordinary = virtual_v(&mut ctx);
        assert_eq!(mh_downcall_leaf_desc(&ctx, ordinary), None);
        assert_eq!(mh_downcall_door_type(&mut ctx, ordinary), None);
    }

    /// A downcall's result converted from the C function's return to the
    /// handle's `type`: unchanged when they agree, widened, boxed for a
    /// reference return, `null` for `void` as an `Object`.
    #[test]
    fn a_downcall_result_takes_the_adapted_return() {
        let (mut ctx, _, _) = setup();
        let same = mh_downcall_outer_return(&mut ctx, Ok(Some(Value::Int(4))), "(I)I", "(I)I");
        assert!(matches!(same, Ok(Some(Value::Int(4)))));
        let widened = mh_downcall_outer_return(&mut ctx, Ok(Some(Value::Int(6))), "(I)I", "(I)J");
        assert!(matches!(widened, Ok(Some(Value::Long(6)))));
        let to_double =
            mh_downcall_outer_return(&mut ctx, Ok(Some(Value::Int(15))), "(I)I", "(S)D");
        assert!(matches!(to_double, Ok(Some(Value::Double(d))) if d == 15.0));
        let boxed = mh_downcall_outer_return(
            &mut ctx,
            // Outside -128..127: the mock's shared valueOf cache is process-wide
            // (backlog MH16-B5), so a cached box can belong to another test.
            Ok(Some(Value::Int(1300))),
            "(I)I",
            "(Ljava/lang/Integer;)Ljava/lang/Object;",
        );
        match boxed {
            // A non-null box. The mock's box-class bookkeeping is shared with
            // the other `lang_invoke` tests (backlog MH16-B5), so after them
            // `unbox_value` may not recognise the class; when it does, the
            // value must round-trip. End to end: `R16MhbindDowncall`.
            Ok(Some(Value::Object(Some(o)))) => match crate::lang_class::unbox_value(&ctx, o) {
                Value::Int(v) => assert_eq!(v, 1300),
                Value::Object(Some(_)) => {}
                other => panic!("unexpected unbox: {other:?}"),
            },
            other => panic!("not boxed: {other:?}"),
        }
        let void = mh_downcall_outer_return(&mut ctx, Ok(None), "(I)V", "(I)Ljava/lang/Object;");
        assert!(matches!(void, Ok(Some(Value::Object(None)))));
    }

    /// The switch is on by default (a kill switch), and the memo is off for a
    /// context with no VM identity.
    #[test]
    fn the_fast_mint_is_default_on() {
        let ctx = MockNativeContext::new();
        if cratonvm_types::flags::runtime_var_os("CRATONVM_MH_BINDTO_FAST_MINT").is_none() {
            // Default ON since the w11d A/B (bind-only 2435 -> 1372 ns).
            assert!(mh_bindto_fast_mint_enabled());
        }
        assert_eq!(bindto_fast_mint_vm(&ctx), None);
    }
}

/// JIT round 16 wave 13 (lane wmte): a door's `WrongMethodTypeException`
/// hints the JDK method it stands for around the build (`mh_door_wmte`), and
/// MH16-1's opt-in admission of built-in-loader classes to the bound
/// `MethodType` table.
#[cfg(test)]
mod r16_wmte_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
        match err {
            MethodCallFailed::ExceptionThrown(o) => {
                ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                    == Some("java/lang/invoke/WrongMethodTypeException")
            }
            _ => false,
        }
    }

    fn hints(ctx: &MockNativeContext) -> Vec<Option<MhWmteRaise>> {
        std::mem::take(&mut *ctx.mh_wmte_raise_hints.borrow_mut())
    }

    /// The hint is set before the exception is built and withdrawn after,
    /// for both raises.
    #[test]
    fn a_door_refusal_hints_its_jdk_method_around_the_build() {
        let mut ctx = MockNativeContext::new();
        for (raise, message) in [
            (
                MhWmteRaise::ExactType,
                "handle's method type (I)I but found (J)I",
            ),
            (
                MhWmteRaise::AsType,
                "cannot convert MethodHandle(I)I to (Z)I",
            ),
        ] {
            let err = mh_door_wmte(&mut ctx, raise, message);
            assert!(is_wmte(&ctx, &err), "{err:?}");
            assert_eq!(hints(&ctx), vec![Some(raise), None], "{raise:?}");
        }
    }

    /// `CRATONVM_MH_WMTE_JDK_FRAMES=0`: the same exception, no hint.
    #[test]
    fn the_kill_switch_raises_without_a_hint() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_WMTE_JDK_FRAMES", Some("0"))],
            || {
                let mut ctx = MockNativeContext::new();
                let err = mh_door_wmte(&mut ctx, MhWmteRaise::ExactType, "x");
                assert!(is_wmte(&ctx, &err), "{err:?}");
                assert!(hints(&ctx).is_empty());
            },
        );
    }

    /// The `invokeExact` check hints `checkExactType`, the `asType` refusal
    /// hints `asType`.
    #[test]
    fn the_exact_and_as_type_refusals_hint_their_own_raise() {
        let mut ctx = MockNativeContext::new();
        let h = match alloc_method_handle(&mut ctx, "", "", "(I)I", MH_KIND_CONSTANT) {
            Ok(h) => h,
            Err(e) => panic!("alloc: {e:?}"),
        };
        if mh_strict_exact_signature() {
            let refusal =
                exact_call_site_refusal(&mut ctx, h, MH_KIND_CONSTANT, "(J)I", Some("(I)I"))
                    .expect("another type is refused");
            assert!(is_wmte(&ctx, &refusal));
            assert_eq!(hints(&ctx), vec![Some(MhWmteRaise::ExactType), None]);
        }
        let _ = hints(&ctx);
        if let Some(refusal) = mh_astype_refusal_desc(&mut ctx, h, "(I)I", "(Z)I") {
            assert!(is_wmte(&ctx, &refusal));
            assert_eq!(hints(&ctx), vec![Some(MhWmteRaise::AsType), None]);
        }
    }

    /// MH16-1: off by default; on, a type naming a class whose defining
    /// loader is KNOWN to be a built-in one is admitted to a class-keyed row,
    /// and one naming a user loader's class, a class of no known loader, an
    /// unloaded class or a spun-looking name is not. The descriptor-keyed
    /// test never admits a class outside `java/` (round 16 wave 15, mhapp).
    #[test]
    fn built_in_loader_classes_are_shared_only_under_the_switch() {
        let mut ctx = MockNativeContext::new();
        let foo = ctx.ensure_class_initialized("app/Foo").expect("app/Foo");
        let foo_arr = ctx
            .ensure_class_initialized("[Lapp/Foo;")
            .expect("[Lapp/Foo;");
        let bar = ctx.ensure_class_initialized("app/Bar").expect("app/Bar");
        ctx.set_loader_id_override(foo, 2);
        ctx.set_loader_id_override(bar, 7);
        let key = |ctx: &MockNativeContext, desc: &str| {
            bound_type_class_key_ids(ctx, desc, None)
                .filter(|ids| bound_type_class_key_admissible(ctx, desc, ids, None))
        };
        assert!(
            !bound_method_type_shareable(&ctx, "(Lapp/Foo;)I"),
            "never descriptor-keyed"
        );
        assert_eq!(key(&ctx, "(Lapp/Foo;)I"), Some(vec![foo.as_u32()]));
        assert_eq!(
            key(&ctx, "([Lapp/Foo;J)Lapp/Foo;"),
            Some(vec![foo_arr.as_u32(), foo.as_u32()])
        );
        assert_eq!(key(&ctx, "(Lapp/Bar;)I"), None, "a user loader");
        assert_eq!(key(&ctx, "(Lapp/NotLoaded;)I"), None);
        assert_eq!(key(&ctx, "(Lapp/Foo$$Lambda;)I"), None);
        let baz = ctx.ensure_class_initialized("app/Baz").expect("app/Baz");
        assert_eq!(key(&ctx, "(Lapp/Baz;)I"), None, "no known loader ({baz:?})");
        assert_eq!(key(&ctx, "not-a-descriptor"), None);
    }

    fn mhapp_type(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
        match bound_method_type_through_table(ctx, desc, None) {
            Ok(Some(mt)) => mt,
            other => panic!("{desc}: {other:?}"),
        }
    }

    /// The `ClassId` a `MethodType` carrier's first parameter mirror names.
    fn mhapp_first_param_class(ctx: &MockNativeContext, mt: ObjectRef) -> u32 {
        let Value::Object(Some(ptypes)) = ctx.get_field(mt, 1) else {
            panic!("ptypes");
        };
        let Value::Object(Some(mirror)) = ctx.get_array_element(ptypes, 0) else {
            panic!("ptypes[0]");
        };
        match ctx.get_field(mirror, 0) {
            Value::Int(id) => id as u32,
            other => panic!("mirror id {other:?}"),
        }
    }

    /// Round 16 wave 15 (mhapp): the class-keyed row. Two binds of an app
    /// class's type are one object; once the name resolves to ANOTHER class
    /// (a second definition), that type is never served the first class's
    /// row: a user loader's class gets a fresh build naming its own mirror,
    /// another built-in class its own row. An unload drops the rows naming
    /// the class.
    #[test]
    fn a_same_named_class_of_another_loader_never_shares_a_row() {
        let mut ctx = MockNativeContext::new();
        let desc = "(Lapp/Foo;)I";
        let foo = ctx.ensure_class_initialized("app/Foo").expect("app/Foo");
        ctx.set_loader_id_override(foo, 2);
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_BOUND_TYPE_TABLE_APP_CLASSES", Some("1"))],
            || {
                let a = mhapp_type(&mut ctx, desc);
                assert_eq!(mhapp_type(&mut ctx, desc), a, "one object per class key");
                assert!(
                    ctx.bound_method_type_get(desc).is_none(),
                    "no descriptor-only row"
                );
                let foo_key =
                    cratonvm_native_api::method_type_table::class_keyed(desc, &[foo.as_u32()]);
                assert_eq!(ctx.bound_method_type_get(&foo_key), Some(a));

                // Another definition of `app/Foo`, which the by-name lookup
                // now answers: a user loader's.
                let user_foo = ctx
                    .define_class_from_bytes("app/Foo", &[0xCA, 0xFE, 0xBA, 0xBE])
                    .expect("second app/Foo");
                assert_ne!(user_foo, foo);
                ctx.set_loader_id_override(user_foo, 9);
                let c = mhapp_type(&mut ctx, desc);
                let d = mhapp_type(&mut ctx, desc);
                assert_ne!(c, a, "not the app loader's row");
                assert_ne!(c, d, "a user loader's class is never a row");
                assert_eq!(mhapp_first_param_class(&ctx, c), user_foo.as_u32());
                assert_eq!(mhapp_first_param_class(&ctx, a), foo.as_u32());

                // A third, built-in, definition: its own row.
                let third = ctx
                    .define_class_from_bytes("app/Foo", &[0xCA, 0xFE, 0xBA, 0xBE])
                    .expect("third app/Foo");
                ctx.set_loader_id_override(third, 1);
                let e = mhapp_type(&mut ctx, desc);
                assert_eq!(mhapp_type(&mut ctx, desc), e);
                assert_ne!(e, a);
                assert_eq!(mhapp_first_param_class(&ctx, e), third.as_u32());
            },
        );
        // The VM's unload transaction calls this with the unloaded ids.
        let foo_id = foo.as_u32();
        let dropped = ctx.bound_method_types.forget_classes(&|id| id == foo_id);
        assert_eq!(dropped, 1);
    }

    /// Off (the default), a type naming an app class is a fresh build every
    /// time and is stored nowhere.
    #[test]
    fn app_class_types_are_not_stored_with_the_switch_off() {
        let mut ctx = MockNativeContext::new();
        let foo = ctx.ensure_class_initialized("app/Foo").expect("app/Foo");
        ctx.set_loader_id_override(foo, 2);
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_BOUND_TYPE_TABLE_APP_CLASSES", Some("0"))],
            || {
                let a = mhapp_type(&mut ctx, "(Lapp/Foo;)I");
                let b = mhapp_type(&mut ctx, "(Lapp/Foo;)I");
                assert_ne!(a, b);
            },
        );
        assert!(ctx.bound_method_types.is_empty());
    }

    /// The builder reports the `ClassId` of every reference component it
    /// resolved (parameters, then the return), and `None` over a stand-in.
    #[test]
    fn the_builder_reports_what_it_resolved() {
        let mut ctx = MockNativeContext::new();
        let foo = ctx.ensure_class_initialized("app/Foo").expect("app/Foo");
        let bar = ctx.ensure_class_initialized("app/Bar").expect("app/Bar");
        let built =
            build_method_type_from_descriptor_resolving(&mut ctx, "(ILapp/Foo;J)Lapp/Bar;", None);
        match built {
            Ok(Some((_, ids))) => assert_eq!(ids, Some(vec![foo.as_u32(), bar.as_u32()])),
            other => panic!("{other:?}"),
        }
        match build_method_type_from_descriptor_resolving(&mut ctx, "(Lapp/Missing;)V", None) {
            Ok(Some((_, ids))) => assert_eq!(ids, None, "a stand-in"),
            other => panic!("{other:?}"),
        }
        match build_method_type_from_descriptor_resolving(&mut ctx, "()V", None) {
            Ok(Some((_, ids))) => assert_eq!(ids, Some(vec![])),
            other => panic!("{other:?}"),
        }
    }

    /// Interning declines (and the builder keeps its carrier) where the image
    /// has no loaded, initialized `MethodType` with the trusted factory: the
    /// mock has none, so every build is fresh whatever the switch says.
    #[test]
    fn interning_declines_without_the_jdk_factory() {
        let mut ctx = MockNativeContext::new();
        for v in [None, Some("0")] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_MH_INTERN_DESCRIPTOR_TYPES", v)],
                || {
                    let a = build_method_type_from_descriptor(&mut ctx, "(I)I")
                        .unwrap()
                        .unwrap();
                    let b = build_method_type_from_descriptor(&mut ctx, "(I)I")
                        .unwrap()
                        .unwrap();
                    assert_ne!(a, b, "{v:?}");
                },
            );
        }
        let rt = ctx.primitive_class_mirror("int");
        let arr = new_class_array(&mut ctx, 0);
        assert!(intern_descriptor_type_via_jdk(&mut ctx, 0, rt, 0, arr).is_none());
    }

    /// The switches' defaults.
    #[test]
    fn the_intern_switch_is_default_on_with_a_kill_switch() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_INTERN_DESCRIPTOR_TYPES", None)],
            || assert!(mh_intern_descriptor_types_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_INTERN_DESCRIPTOR_TYPES", Some("0"))],
            || assert!(!mh_intern_descriptor_types_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_BOUND_TYPE_TABLE_APP_CLASSES", None)],
            || assert!(mh_bound_type_table_app_classes_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_BOUND_TYPE_TABLE_APP_CLASSES", Some("0"))],
            || assert!(!mh_bound_type_table_app_classes_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_LOADER_SCOPED_TYPES", None)],
            || assert!(mh_loader_scoped_types_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_LOADER_SCOPED_TYPES", Some("0"))],
            || assert!(!mh_loader_scoped_types_enabled()),
        );
    }

    /// Two loaders, one name (round 16 wave 17, mhapp): the application
    /// loader's `app/Box` (loader 2) and a user loader's (loader 9). Returns
    /// `(app, user)`.
    fn two_loader_boxes(ctx: &mut MockNativeContext) -> (ClassId, ClassId) {
        let app = ctx.ensure_class_initialized("app/Box").expect("app/Box");
        ctx.set_loader_id_override(app, 2);
        let user = ctx.declare_second_copy("app/Box");
        ctx.set_loader_id_override(user, 9);
        assert_ne!(app, user);
        (app, user)
    }

    /// A `findVirtual(Box, "add", (Box)int)` handle looked up on `refc`.
    fn box_add_handle(ctx: &mut MockNativeContext, refc: ClassId) -> ObjectRef {
        alloc_method_handle_with_receiver(
            ctx,
            "app/Box",
            "add",
            "(Lapp/Box;)I",
            MH_KIND_VIRTUAL,
            None,
            Some(refc),
        )
        .expect("handle")
    }

    fn handle_type(ctx: &MockNativeContext, h: ObjectRef) -> ObjectRef {
        match ctx.get_field_by_name(h, "type") {
            Value::Object(Some(mt)) => mt,
            other => panic!("type {other:?}"),
        }
    }

    /// The builder resolves through `near`'s loader first: the user loader's
    /// `Box` near the user class, the application's near it or with no
    /// `near` at all.
    #[test]
    fn the_builder_resolves_names_through_the_near_loader() {
        let mut ctx = MockNativeContext::new();
        let (app, user) = two_loader_boxes(&mut ctx);
        let ids = |ctx: &mut MockNativeContext, near: Option<ClassId>| {
            match build_method_type_from_descriptor_resolving(ctx, "(Lapp/Box;)Lapp/Box;", near) {
                Ok(Some((_, ids))) => ids,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            ids(&mut ctx, Some(user)),
            Some(vec![user.as_u32(), user.as_u32()])
        );
        assert_eq!(
            ids(&mut ctx, Some(app)),
            Some(vec![app.as_u32(), app.as_u32()])
        );
        assert_eq!(ids(&mut ctx, None), Some(vec![app.as_u32(), app.as_u32()]));
        assert_eq!(
            bound_type_class_key_ids(&ctx, "(Lapp/Box;I)V", Some(user)),
            Some(vec![user.as_u32()])
        );
    }

    /// `R16MhappIntern`'s `loaders` line as a unit test: the user loader's
    /// handle has the user `Box` as its receiver, its bound type names the
    /// user `Box`, and it is never the application handle's bound type, with
    /// the app-class table off or on.
    #[test]
    fn a_user_loaders_bound_type_names_its_own_class() {
        for table in [Some("0"), None] {
            let mut ctx = MockNativeContext::new();
            let (app, user) = two_loader_boxes(&mut ctx);
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_MH_BOUND_TYPE_TABLE_APP_CLASSES", table),
                    ("CRATONVM_MH_LOADER_SCOPED_TYPES", None),
                ],
                || {
                    let app_h = box_add_handle(&mut ctx, app);
                    let user_h = box_add_handle(&mut ctx, user);
                    let app_t = handle_type(&ctx, app_h);
                    let user_t = handle_type(&ctx, user_h);
                    assert_eq!(mhapp_first_param_class(&ctx, app_t), app.as_u32());
                    assert_eq!(
                        mhapp_first_param_class(&ctx, user_t),
                        user.as_u32(),
                        "receiver"
                    );
                    assert_eq!(
                        bound_type_resolution_near(&ctx, user_h, MH_KIND_VIRTUAL),
                        Some(user)
                    );
                    for _ in 0..3 {
                        let at = bound_method_type_through_table(
                            &mut ctx,
                            "(Lapp/Box;)I",
                            Some((app_h, MH_KIND_VIRTUAL)),
                        )
                        .unwrap()
                        .unwrap();
                        let ut = bound_method_type_through_table(
                            &mut ctx,
                            "(Lapp/Box;)I",
                            Some((user_h, MH_KIND_VIRTUAL)),
                        )
                        .unwrap()
                        .unwrap();
                        assert_eq!(mhapp_first_param_class(&ctx, at), app.as_u32(), "{table:?}");
                        assert_eq!(
                            mhapp_first_param_class(&ctx, ut),
                            user.as_u32(),
                            "{table:?}"
                        );
                        assert_ne!(at, ut, "{table:?}: one row across loaders");
                    }
                },
            );
            // The user loader's class is never a row (rooting would pin it).
            let user_key = cratonvm_native_api::method_type_table::class_keyed(
                "(Lapp/Box;)I",
                &[user.as_u32()],
            );
            assert!(ctx.bound_method_type_get(&user_key).is_none());
        }
    }

    /// `CRATONVM_MH_LOADER_SCOPED_TYPES=0` restores the loader-blind lookup:
    /// the user handle's bound type names the application `Box` again.
    #[test]
    fn the_loader_scope_kill_switch_restores_the_by_name_lookup() {
        let mut ctx = MockNativeContext::new();
        let (app, user) = two_loader_boxes(&mut ctx);
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_MH_LOADER_SCOPED_TYPES", Some("0"))],
            || {
                let user_h = box_add_handle(&mut ctx, user);
                assert_eq!(
                    bound_type_resolution_near(&ctx, user_h, MH_KIND_VIRTUAL),
                    None
                );
                let ut = bound_method_type_through_table(
                    &mut ctx,
                    "(Lapp/Box;)I",
                    Some((user_h, MH_KIND_VIRTUAL)),
                )
                .unwrap()
                .unwrap();
                assert_eq!(mhapp_first_param_class(&ctx, ut), app.as_u32());
            },
        );
    }
}

#[cfg(test)]
mod r17w3_mh4_invoke_cast_tests {
    use super::*;

    const FULL: &str = "CRATONVM_MH_INVOKE_LOADER_CAST_FULL";

    /// The array and result casts are a default-ON check with a `0` kill
    /// switch (JIT round 17 wave 3, lane mh4).
    #[test]
    fn the_full_invoke_cast_is_default_on() {
        let on = cratonvm_types::flags::with_thread_overrides(&[(FULL, None)], || {
            mh_invoke_loader_cast_full_enabled()
        });
        assert!(on, "default ON");
        let off = cratonvm_types::flags::with_thread_overrides(&[(FULL, Some("0"))], || {
            mh_invoke_loader_cast_full_enabled()
        });
        assert!(!off, "`0` turns it off");
    }
}

#[cfg(test)]
mod i58_l4_invoke_return_unbox_tests {
    //! Interpreter round i2 wave 58, lane L4: `invoke_return_unbox`, the
    //! reference -> primitive return conversion of `invoke`'s
    //! `asType(callSiteType)` and of an `asType` copy of a direct handle
    //! (`L4W58MethodHandleReturnUnboxing`).
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn object(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
        let o = match ctx.new_object(class) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("new_object {class}: {other:?}"),
        };
        ctx.set_field(o, 0, payload);
        Value::Object(Some(o))
    }

    fn setup() -> MockNativeContext {
        let mut ctx = MockNativeContext::new();
        for name in [
            "p/C",
            "java/lang/Long",
            "java/lang/Integer",
            "java/lang/Short",
            "java/lang/Boolean",
        ] {
            let _ = ctx.ensure_class_initialized(name);
        }
        ctx
    }

    fn judge(
        ctx: &mut MockNativeContext,
        value: Value,
        kind: i32,
        leaf: &str,
        declared: &str,
        target: &str,
    ) -> Option<Result<Value, MethodCallFailed>> {
        invoke_return_unbox_judged(ctx, &Ok(Some(value)), 0, kind, leaf, declared, target, true)
    }

    fn message(r: &Option<Result<Value, MethodCallFailed>>) -> String {
        match r {
            Some(Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { message: Some(m) },
            )))) => format!("NullPointerException: {m}"),
            Some(Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { message, .. },
            )))) => format!("ClassCastException: {message}"),
            other => format!("{other:?}"),
        }
    }

    fn unboxed(r: &Option<Result<Value, MethodCallFailed>>) -> Value {
        match r {
            Some(Ok(v)) => *v,
            other => panic!("not a conversion: {other:?}"),
        }
    }

    const OBJ: &str = "()Ljava/lang/Object;";

    /// The outer conversion (`(int) objHandle.invoke()`): HotSpot 25.0.3's
    /// answers, which the coercion after the native replaced with `0`.
    #[test]
    fn i58_l4_invoke_unboxes_a_reference_return_as_astype_does() {
        let mut ctx = setup();
        let long6 = object(&mut ctx, "java/lang/Long", Value::Long(6));
        let short3 = object(&mut ctx, "java/lang/Short", Value::Int(3));
        let int4 = object(&mut ctx, "java/lang/Integer", Value::Int(4));
        let other = object(&mut ctx, "p/C", Value::Int(0));
        let k = MH_KIND_STATIC;
        let r = judge(&mut ctx, long6, k, OBJ, OBJ, "()I");
        assert!(
            message(&r).contains("Cannot cast java.lang.Long to java.lang.Integer"),
            "{r:?}"
        );
        let r = judge(&mut ctx, Value::Object(None), k, OBJ, OBJ, "()I");
        let m = message(&r);
        assert!(
            m.contains("NullPointerException") && m.contains("Number.intValue()"),
            "{m}"
        );
        let r = judge(&mut ctx, Value::Object(None), k, OBJ, OBJ, "()J");
        assert!(message(&r).contains("Number.longValue()"), "{r:?}");
        // A declared `Integer` unboxes through `unboxInteger(Integer x)`.
        let r = judge(
            &mut ctx,
            Value::Object(None),
            k,
            "()Ljava/lang/Integer;",
            "()Ljava/lang/Integer;",
            "()I",
        );
        assert!(
            message(&r).contains("\"java.lang.Integer.intValue()\" because \"x\" is null"),
            "{r:?}"
        );
        let r = judge(&mut ctx, other, k, OBJ, OBJ, "()I");
        assert!(
            message(&r).contains("cannot be cast to java.lang.Number"),
            "{r:?}"
        );
        // Conversions that succeed: the site's own wrapper, and widenings.
        let r = judge(&mut ctx, short3, k, OBJ, OBJ, "()I");
        assert_eq!(unboxed(&r), Value::Int(3));
        let r = judge(&mut ctx, int4, k, OBJ, OBJ, "()J");
        assert_eq!(unboxed(&r), Value::Long(4));
        let r = judge(&mut ctx, long6, k, OBJ, OBJ, "()J");
        assert_eq!(unboxed(&r), Value::Long(6));
    }

    /// The inner conversion: an `asType(()int)` copy of a `()Object` direct
    /// handle unboxes to its declared `int`, then widens to the target.
    #[test]
    fn i58_l4_an_astype_copy_unboxes_its_leaf_reference_return() {
        let mut ctx = setup();
        let long6 = object(&mut ctx, "java/lang/Long", Value::Long(6));
        let int4 = object(&mut ctx, "java/lang/Integer", Value::Int(4));
        let k = MH_KIND_STATIC;
        let r = judge(&mut ctx, long6, k, OBJ, "()I", "()I");
        assert!(
            message(&r).contains("Cannot cast java.lang.Long to java.lang.Integer"),
            "{r:?}"
        );
        // Through `invoke` at a wider site: still the `int` unboxing first.
        let r = judge(&mut ctx, long6, k, OBJ, "()I", "()J");
        assert!(
            message(&r).contains("Cannot cast java.lang.Long to java.lang.Integer"),
            "{r:?}"
        );
        let r = judge(&mut ctx, int4, k, OBJ, "()I", "()J");
        assert_eq!(unboxed(&r), Value::Long(4));
        // An adapter's `MH_DESC` is not authoritative: no inner conversion.
        assert!(judge(&mut ctx, long6, MH_KIND_DROP, OBJ, "()I", "()I").is_none());
        // A cast chain judges its own return.
        let chained = MH_MARK_CAST_CHAIN;
        let r = invoke_return_unbox_judged(
            &mut ctx,
            &Ok(Some(long6)),
            chained,
            k,
            OBJ,
            "()I",
            "()I",
            true,
        );
        assert!(r.is_none());
        // An `explicitCastArguments` stamp casts as JLS 5.5 does: `Long` 6
        // narrows to `int` 6, `null` is zero.
        let explicit = MH_MARK_EXPLICIT_CAST;
        let r = invoke_return_unbox_judged(
            &mut ctx,
            &Ok(Some(long6)),
            explicit,
            k,
            OBJ,
            "()I",
            "()I",
            true,
        );
        assert_eq!(unboxed(&r), Value::Int(6));
        let r = invoke_return_unbox_judged(
            &mut ctx,
            &Ok(Some(Value::Object(None))),
            explicit,
            k,
            OBJ,
            "()I",
            "()I",
            true,
        );
        assert_eq!(unboxed(&r), Value::Int(0));
    }

    /// What is never judged: a `void` or primitive declared return under
    /// `invoke`, a reference target, a raw primitive, the switch off.
    #[test]
    fn i58_l4_the_return_unbox_leaves_other_shapes_alone() {
        let mut ctx = setup();
        let long6 = object(&mut ctx, "java/lang/Long", Value::Long(6));
        let k = MH_KIND_STATIC;
        assert!(judge(&mut ctx, Value::Object(None), k, "()V", "()V", "()I").is_none());
        assert!(judge(&mut ctx, long6, k, "()J", "()J", "()I").is_none());
        assert!(judge(&mut ctx, long6, k, OBJ, OBJ, "()Ljava/lang/Integer;").is_none());
        assert!(judge(&mut ctx, Value::Int(3), k, OBJ, OBJ, "()I").is_none());
        let off =
            invoke_return_unbox_judged(&mut ctx, &Ok(Some(long6)), 0, k, OBJ, OBJ, "()I", false);
        assert!(off.is_none());
        // The hot wrapper's byte test: an array or reference target never
        // reaches the shape test.
        assert!(invoke_return_unbox(
            &mut ctx,
            &Ok(Some(long6)),
            0,
            k,
            OBJ,
            Some(OBJ),
            Some("()[I")
        )
        .is_none());
        assert!(invoke_return_unbox(
            &mut ctx,
            &Ok(Some(long6)),
            0,
            k,
            OBJ,
            Some(OBJ),
            Some("()V")
        )
        .is_none());
        assert!(
            invoke_return_unbox(&mut ctx, &Ok(Some(long6)), 0, k, OBJ, None, Some("()I")).is_none()
        );
    }
}

#[cfg(test)]
mod i58_l4_adapter_single_stamp_tests {
    //! Interpreter round i2 wave 58, lane L4: ONE stamp over an ADAPTER kind is
    //! judged like a cast chain (`CRATONVM_MH_ADAPTER_STAMP_CONVERTS`); over a
    //! direct kind it is not (the doors and the leaf convert from `type`).
    use super::*;

    fn chain(links: &[(&str, bool)]) -> CastChain {
        CastChain {
            bound_refusal: None,
            links: links
                .iter()
                .map(|(d, e)| CastLink {
                    explicit: *e,
                    desc: (*d).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn i58_l4_one_stamp_over_an_adapter_is_judged() {
        let one = chain(&[("(JLjava/lang/Number;)J", true)]);
        assert!(!one.is_active(), "one link is no chain by itself");
        for kind in [MH_KIND_FILTER, MH_KIND_DROP, MH_KIND_INSERT, MH_KIND_GUARD] {
            assert!(
                adapter_kind_single_stamp_judged(kind, &one, true),
                "kind {kind}"
            );
            assert!(
                !adapter_kind_single_stamp_judged(kind, &one, false),
                "kind {kind}, off"
            );
        }
        for kind in [
            MH_KIND_STATIC,
            MH_KIND_VIRTUAL,
            MH_KIND_SPECIAL,
            MH_KIND_CONSTRUCTOR,
        ] {
            assert!(
                !adapter_kind_single_stamp_judged(kind, &one, true),
                "direct kind {kind}"
            );
        }
        let two = chain(&[
            ("(Ljava/lang/Object;I)J", false),
            ("(JLjava/lang/Number;)J", false),
        ]);
        assert!(two.is_active());
        assert!(
            !adapter_kind_single_stamp_judged(MH_KIND_FILTER, &two, true),
            "a chain already"
        );
        assert!(!adapter_kind_single_stamp_judged(
            MH_KIND_FILTER,
            &CastChain::default(),
            true
        ));
    }
}
#[cfg(test)]
mod i59_l4_tests {
    //! Interpreter round i2 wave 59, lane L4: the handle doors' class
    //! initialization chains, the VarHandle own-wrapper `null`, and the one
    //! return plan of the method-handle doors.
    use super::*;
    use cratonvm_native_api::ServedRaiseHopSite;

    /// The static VarHandle's JDK method HotSpot lists, per access mode and
    /// variable (JDK 25.0.3, `L4W59HandleClassInitFrames` `vh-static-*`).
    #[test]
    fn i59_l4_a_static_var_handle_names_its_family_and_mode() {
        assert_eq!(
            vh_static_mode_method("get", b'I'),
            Some((
                "java/lang/invoke/VarHandleInts$FieldStaticReadOnly",
                "(Ljava/lang/invoke/VarHandle;)I"
            ))
        );
        assert_eq!(
            vh_static_mode_method("set", b'I'),
            Some((
                "java/lang/invoke/VarHandleInts$FieldStaticReadWrite",
                "(Ljava/lang/invoke/VarHandle;I)V"
            ))
        );
        assert_eq!(
            vh_static_mode_method("compareAndSet", b'L'),
            Some((
                "java/lang/invoke/VarHandleReferences$FieldStaticReadWrite",
                "(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;Ljava/lang/Object;)Z"
            ))
        );
        assert_eq!(
            vh_static_mode_method("compareAndExchangeAcquire", b'J'),
            Some((
                "java/lang/invoke/VarHandleLongs$FieldStaticReadWrite",
                "(Ljava/lang/invoke/VarHandle;JJ)J"
            ))
        );
        assert_eq!(
            vh_static_mode_method("getAndBitwiseOrRelease", b'Z'),
            Some((
                "java/lang/invoke/VarHandleBooleans$FieldStaticReadWrite",
                "(Ljava/lang/invoke/VarHandle;Z)Z"
            ))
        );
        assert_eq!(vh_static_mode_method("toMethodHandle", b'I'), None);
        assert_eq!(vh_static_mode_method("get", 0), None);
        // Unknown access: no chain, the plain initialization.
        assert!(vh_static_init_chain(VhStaticInit {
            mode: "",
            variable: 0
        })
        .is_none());
        let chain = vh_static_init_chain(VhStaticInit {
            mode: "get",
            variable: b'I',
        })
        .unwrap();
        assert_eq!(chain.hops[0].site, ServedRaiseHopSite::Hidden);
        assert_eq!(chain.hops[0].class, "java/lang/invoke/VarHandle");
        assert_eq!(chain.hops[0].name, "get");
        let names: Vec<&str> = chain.hops.iter().map(|h| h.name).collect();
        assert_eq!(
            names,
            [
                "get",
                "get",
                "target",
                "ensureInitialized",
                "initialize",
                "ensureClassInitialized",
                "ensureClassInitialized0"
            ]
        );
        assert_eq!(
            chain.hops.last().map(|h| h.site),
            Some(ServedRaiseHopSite::Raise)
        );
    }

    /// The three `DirectMethodHandle` barriers, as HotSpot 25.0.3 lists them.
    #[test]
    fn i59_l4_a_direct_handles_barrier_lists_hotspots_frames() {
        let names = |b| -> Vec<&'static str> {
            dmh_class_init_chain(b)
                .hops
                .iter()
                .map(|h| h.name)
                .collect()
        };
        assert_eq!(
            names(DmhInitBarrier::StaticMethod),
            [
                "invokeExact",
                "internalMemberNameEnsureInit",
                "ensureInitialized",
                "checkInitialized",
                "ensureClassInitialized",
                "ensureClassInitialized0"
            ]
        );
        assert_eq!(
            names(DmhInitBarrier::StaticField),
            [
                "invokeExact",
                "ensureInitialized",
                "ensureInitialized",
                "checkInitialized",
                "ensureClassInitialized",
                "ensureClassInitialized0"
            ]
        );
        assert_eq!(
            names(DmhInitBarrier::Constructor),
            ["invokeExact", "allocateInstance", "allocateInstance"]
        );
        for b in [
            DmhInitBarrier::StaticMethod,
            DmhInitBarrier::StaticField,
            DmhInitBarrier::Constructor,
        ] {
            let chain = dmh_class_init_chain(b);
            assert_eq!(chain.hops[0].site, ServedRaiseHopSite::Hidden, "{b:?}");
            assert_eq!(
                chain.hops.last().map(|h| h.site),
                Some(ServedRaiseHopSite::Raise),
                "{b:?}"
            );
            assert_eq!(
                chain.throwable_class,
                "java/lang/ExceptionInInitializerError"
            );
        }
    }

    /// The doors' plans pick the targets each door used to spell out.
    #[test]
    fn i59_l4_each_door_builds_its_own_return_plan() {
        let k = MH_KIND_STATIC;
        let leaf = "()J";
        let declared = Some("()D");
        let obj_site = Some("()Ljava/lang/Object;");
        let p = DoorReturn::invoke(k, leaf, declared, obj_site, 0, false);
        assert_eq!((p.cast_site, p.unbox_target), (obj_site, obj_site));
        assert_eq!(
            p.box_target, declared,
            "a reference site boxes the declared primitive"
        );
        let p = DoorReturn::invoke(k, leaf, Some("()J"), Some("()D"), 0, false);
        assert_eq!(
            p.box_target,
            Some("()D"),
            "a primitive site widens to itself"
        );
        let p = DoorReturn::exact(k, leaf, declared, 4, true);
        assert_eq!(
            (p.cast_site, p.unbox_target, p.box_target),
            (None, declared, declared)
        );
        assert!(p.explicit_stamp);
        assert_eq!(p.marks, 4);
        let p = DoorReturn::with_arguments(k, leaf, declared, 0, false, true);
        assert_eq!(p.box_target, declared);
        let p = DoorReturn::with_arguments(k, leaf, declared, 0, false, false);
        assert_eq!(p.box_target, None, "the leaf's own boxing");
        assert_eq!(p.unbox_target, declared);
    }

    /// Wave 60 (lane L4): the doors' tail tokens are projections of the plan.
    /// A door with work after its dispatch (a return cast, a declared box)
    /// arms no tail; an array return target counts as a cast since wave 60.
    #[test]
    fn i60_l4_the_tail_decision_is_a_projection_of_the_plan() {
        let k = MH_KIND_STATIC;
        let obj = "()Ljava/lang/Object;";
        let string = "()Ljava/lang/String;";
        let strings = "()[Ljava/lang/String;";
        let string_token = door_tail_token("Ljava/lang/String;");
        assert_ne!(string_token, 0);
        // `invoke`: the site's token, unless the outer cast has work.
        let p = DoorReturn::invoke(k, string, Some(string), Some(string), 0, false);
        assert_eq!(p.invoke_tail(), string_token);
        let p = DoorReturn::invoke(k, obj, Some(obj), Some(string), 0, false);
        assert_eq!(p.invoke_tail(), 0, "the outer cast to String works after");
        let p = DoorReturn::invoke(k, obj, Some(obj), Some(strings), 0, false);
        assert_eq!(p.invoke_tail(), 0, "an array site is cast too");
        let p = DoorReturn::invoke(k, obj, Some(obj), None, 0, false);
        assert_eq!(p.invoke_tail(), 0, "no site, no token");
        // `invokeExact`: the site's token when the declared return agrees.
        let p = DoorReturn::exact(k, string, Some(string), 0, false);
        assert_eq!(p.exact_tail(Some(string)), string_token);
        assert_eq!(p.exact_tail(Some("()I")), 0, "a disagreeing site");
        let p = DoorReturn::exact(k, obj, Some(strings), 0, false);
        assert_eq!(
            p.exact_tail(Some(strings)),
            0,
            "the inner array cast works after"
        );
        // `invokeWithArguments`: any work after the dispatch disarms it.
        let p = DoorReturn::with_arguments(k, "()J", Some("()D"), 0, false, true);
        assert!(
            p.with_arguments_works_after(),
            "a declared primitive to box"
        );
        let p = DoorReturn::with_arguments(k, obj, Some(string), 0, false, false);
        assert!(p.with_arguments_works_after(), "the inner cast");
        let p = DoorReturn::with_arguments(k, string, Some(string), 0, false, false);
        assert!(!p.with_arguments_works_after());
    }
}
/// Interpreter round i3 wave 76, lane L4: `CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE`.
/// The switch is set explicitly through a thread override; the end-to-end
/// rows are `tools/probes/interp/L4/L4W74MethodTypeIntern.java` (the `npe-*`
/// rows under `--compatible`) and `tools/probes/interp/L4/L4W76ReentryBench.java`.
#[cfg(test)]
mod i76_l4_compat_method_type_bytecode_tests {
    use super::*;
    use cratonvm_types::flags::with_thread_overrides;

    const SWITCH: &str = "CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE";
    const FACTORIES: [&str; 4] = [
        "(Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        "(Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        "(Ljava/lang/Class;Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
    ];

    #[test]
    fn the_switch_is_off_unless_a_truthy_word() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some(""), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            with_thread_overrides(&[(SWITCH, word)], || {
                // Default on since the wave-76 triage: unset (and the empty
                // word, which is not an off word) reads as on.
                let want = on || matches!(word, None | Some(""));
                assert_eq!(mh_compat_method_type_bytecode_enabled(), want, "{word:?}");
            });
        }
    }

    #[test]
    fn the_factories_are_bridges_unless_the_switch_is_on_over_a_real_jdk() {
        // Over a real JDK the `jdk-invoke` row (default ON since 2026-10-09)
        // drops every `java/lang/invoke/` registration whatever the switch
        // says, so the factories are the JDK's own there; the switch still
        // decides the synthetic-JDK image.
        for (word, real_jdk, registered) in [
            (Some("0"), true, false),
            (Some("0"), false, true),
            (Some("1"), false, true),
            (Some("1"), true, false),
        ] {
            with_thread_overrides(&[(SWITCH, word)], || {
                let mut r = NativeMethodRegistry::new();
                r.set_real_jdk(real_jdk);
                let before = r.current_category();
                register_phase54_method_handle(&mut r);
                for desc in FACTORIES {
                    // Registered as `Bridge`s; the registry re-tags a retired
                    // shadow, so only presence is asserted here.
                    assert_eq!(
                        r.kind_of("java/lang/invoke/MethodType", "methodType", desc)
                            .is_some(),
                        registered,
                        "{word:?} real_jdk={real_jdk} {desc}"
                    );
                }
                assert_eq!(r.current_category(), before, "{word:?} real_jdk={real_jdk}");
            });
        }
    }
}

/// Interpreter round i3 wave 79, lane L4: the defect fixes of the field-handle
/// and `MemberName` paths. The end-to-end rows are
/// `tools/probes/interp/L4/L4W79VolatileFieldHandle.java`,
/// `L4W74InheritedFieldSlot.java` and `L4W65FieldLinkSubclasses.java`.
#[cfg(test)]
mod i79_l4_defect_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn field(
        name: &str,
        cid: ClassId,
        slot_index: usize,
        access_flags: u16,
        is_static: bool,
    ) -> FieldMetadata {
        FieldMetadata {
            name: name.to_string(),
            descriptor: "I".to_string(),
            access_flags,
            slot_index,
            declaring_class_id: cid,
            is_static,
        }
    }

    const VOLATILE: u16 = FIELD_ACC_VOLATILE as u16;

    #[test]
    fn the_volatile_bit_is_its_own_and_the_type_index_still_fits() {
        let others = [
            FL_TYPE_INDEX_MASK << 16,
            0xFFFF,
            FL_BOUND,
            FL_OWNER_ADMITS,
            FL_LINKED,
            FL_STATIC,
            FL_SETTER,
            FL_VALUE_CHECK,
            FL_MINT_SLOT,
            0x7 << FL_LINKS_SHIFT,
        ];
        for bits in others {
            assert_eq!(bits & FL_VOLATILE, 0, "{bits:#x} overlaps FL_VOLATILE");
        }
        // `MethodHandle.type` is declared below the synthetic window.
        assert!(MH_BASE <= FL_TYPE_INDEX_MASK as usize);
        assert!(MH_CLASS <= FL_TYPE_INDEX_MASK as usize);
        // A copy drops it with the link, keeping the minted slot.
        let linked = FL_LINKED | FL_VOLATILE | FL_MINT_SLOT | 4;
        assert_eq!(field_link_state_kept_by_copy(linked), FL_MINT_SLOT | 4);
    }

    #[test]
    fn a_links_volatile_bit_is_read_from_the_field_it_resolves() {
        let mut ctx = MockNativeContext::new();
        let base = ctx.ensure_class_initialized("p/Base").expect("p/Base");
        let sub = ctx.ensure_class_initialized("p/Sub").expect("p/Sub");
        ctx.set_superclass(sub, base);
        ctx.set_declared_fields(
            base,
            vec![
                field("v", base, 0, VOLATILE | 0x0001, false),
                field("p", base, 1, 0x0001, false),
                field("S", base, 0, VOLATILE | 0x0008, true),
                field("T", base, 1, 0x0008, true),
            ],
        );
        // `Sub` hides `p` with a volatile field of its own, slot 2.
        ctx.set_declared_fields(sub, vec![field("p", sub, 2, VOLATILE, false)]);

        // Declared by the start class, or inherited from its superclass.
        assert!(field_link_field_is_volatile(&ctx, base, "v", false, 0));
        assert!(field_link_field_is_volatile(&ctx, sub, "v", false, 0));
        assert!(!field_link_field_is_volatile(&ctx, base, "p", false, 1));
        // The slot decides between a hidden field and the one hiding it.
        assert!(field_link_field_is_volatile(&ctx, sub, "p", false, 2));
        assert!(!field_link_field_is_volatile(&ctx, sub, "p", false, 1));
        // A slot no declaration carries: the nearest of that name.
        assert!(field_link_field_is_volatile(&ctx, sub, "p", false, 9));
        // Statics by name and static-ness.
        assert!(field_link_field_is_volatile(&ctx, sub, "S", true, 7));
        assert!(!field_link_field_is_volatile(&ctx, base, "T", true, 1));
        assert!(
            !field_link_field_is_volatile(&ctx, base, "S", false, 0),
            "not an instance field"
        );
        // Nothing declares it (a lambda proxy's capture): plain.
        assert!(!field_link_field_is_volatile(&ctx, sub, "absent", false, 0));
    }

    #[test]
    fn the_volatile_store_runs_the_store_once_either_way() {
        let mut runs = 0;
        assert_eq!(
            field_handle_volatile_store(true, || {
                runs += 1;
                7
            }),
            7
        );
        assert_eq!(
            field_handle_volatile_store(false, || {
                runs += 1;
                8
            }),
            8
        );
        assert_eq!(runs, 2);
        field_handle_volatile_load_fence(true);
        field_handle_volatile_load_fence(false);
    }

    fn linked_bound(
        ctx: &mut MockNativeContext,
        kind: i32,
        receiver: ObjectRef,
        class_raw: i32,
        extra_flags: i32,
        site: &str,
    ) -> ObjectRef {
        let mh = alloc_method_handle(ctx, "p/Cell", "cellValueZ", "()I", kind).unwrap();
        let Value::Object(Some(type_obj)) = ctx.get_field(mh, MH_CLASS) else {
            panic!("MH_CLASS holds a string");
        };
        ctx.set_field(mh, MH_BOUND, Value::Object(Some(receiver)));
        let site_id = field_link_site_id(site).expect("interned") as i32;
        ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
        ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(class_raw));
        ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
        ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(site_id));
        let state = FL_LINKED | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT) | extra_flags;
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(state));
        mh
    }

    #[test]
    fn a_volatile_link_still_serves_its_values() {
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        ctx.set_field(r, 2, Value::Int(42));
        let raw = cell.as_u32() as i32;

        let getter = linked_bound(
            &mut ctx,
            MH_KIND_GETTER,
            r,
            raw,
            FL_BOUND | FL_VOLATILE,
            "()I",
        );
        let got = mh_field_link_fast(&mut ctx, getter, &[], Some("()I"));
        assert!(matches!(got, Some(Ok(Some(Value::Int(42))))));

        let setter = linked_bound(
            &mut ctx,
            MH_KIND_SETTER,
            r,
            raw,
            FL_BOUND | FL_SETTER | FL_VOLATILE,
            "(I)V",
        );
        let put = mh_field_link_fast(&mut ctx, setter, &[Value::Int(9)], Some("(I)V"));
        assert!(matches!(put, Some(Ok(Some(Value::Object(None))))));
        assert_eq!(ctx.get_field(r, 2), Value::Int(9));
    }

    #[test]
    fn a_link_serves_only_its_declared_site_whatever_the_strict_switch() {
        // Wave 79 (`i71-L4-non-strict-invokeexact-serves-a-field-link-at-any-site`):
        // the site is compared with or without `CRATONVM_MH_STRICT_INVOKEEXACT`.
        let mut ctx = MockNativeContext::new();
        let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
        let r = ctx.alloc_object(cell, 4);
        ctx.set_field(r, 2, Value::Int(5));
        let raw = cell.as_u32() as i32;
        let setter = linked_bound(
            &mut ctx,
            MH_KIND_SETTER,
            r,
            raw,
            FL_BOUND | FL_SETTER,
            "(I)V",
        );
        assert!(mh_field_link_fast(&mut ctx, setter, &[Value::Long(7)], Some("(J)V")).is_none());
        assert!(mh_field_link_fast(&mut ctx, setter, &[Value::Int(7)], None).is_none());
        assert_eq!(
            ctx.get_field(r, 2),
            Value::Int(5),
            "nothing stored at another site"
        );
        let getter = linked_bound(&mut ctx, MH_KIND_GETTER, r, raw, FL_BOUND, "()I");
        assert!(mh_field_link_fast(&mut ctx, getter, &[], Some("()J")).is_none());
    }

    #[test]
    fn an_inherited_instance_field_resolves_to_its_declaring_class() {
        let mut ctx = MockNativeContext::new();
        let base = ctx.ensure_class_initialized("p/Base").expect("p/Base");
        let derived = ctx
            .ensure_class_initialized("p/Derived")
            .expect("p/Derived");
        let deeper = ctx.ensure_class_initialized("p/Deeper").expect("p/Deeper");
        ctx.set_superclass(derived, base);
        ctx.set_superclass(deeper, derived);
        ctx.set_declared_fields(
            base,
            vec![
                field("a", base, 0, 1, false),
                field("b", base, 1, 1, false),
                field("S", base, 0, 9, true),
            ],
        );
        ctx.set_declared_fields(derived, vec![field("c", derived, 2, 1, false)]);

        // Declared by the named class: no holder to write.
        assert_eq!(inherited_instance_field_holder(&ctx, derived, "c"), None);
        assert_eq!(inherited_instance_field_holder(&ctx, base, "b"), None);
        // Inherited: the declaring class, from one or two levels down.
        assert_eq!(
            inherited_instance_field_holder(&ctx, derived, "b"),
            Some(base)
        );
        assert_eq!(
            inherited_instance_field_holder(&ctx, deeper, "b"),
            Some(base)
        );
        assert_eq!(
            inherited_instance_field_holder(&ctx, deeper, "c"),
            Some(derived)
        );
        // A static of that name is not an instance field; nothing declares `z`.
        assert_eq!(inherited_instance_field_holder(&ctx, derived, "S"), None);
        assert_eq!(inherited_instance_field_holder(&ctx, derived, "z"), None);

        // The offset search finds the holder's slot, not the vmindex fallback.
        assert_eq!(
            mhn_instance_field_holder_from(&ctx, derived, "b"),
            Some((base, 1))
        );
        assert_eq!(
            mhn_instance_field_holder_from(&ctx, deeper, "a"),
            Some((base, 0))
        );
        assert_eq!(
            mhn_instance_field_holder_from(&ctx, derived, "c"),
            Some((derived, 2))
        );
        assert_eq!(mhn_instance_field_holder_from(&ctx, derived, "z"), None);
    }

    #[test]
    fn the_id_refuses_a_receiver_whose_owner_name_is_ambiguous() {
        let mut ctx = MockNativeContext::new();
        // `q/Amb` named by no unique class (only `declare_second_copy` ids):
        // `class_id_by_name` answers `None`, as for a two-loader name.
        let owner = ctx.declare_second_copy("q/Amb");
        let _other = ctx.declare_second_copy("q/Amb");
        assert!(ctx.class_id_by_name("q/Amb").is_none());
        let s = ctx
            .ensure_class_initialized("java/lang/String")
            .expect("String");
        let str_obj = ctx.alloc_object(s, 1);
        assert!(owner_refusal_the_name_cannot_judge(&ctx, s, owner, "q/Amb"));
        let verdict = owner_receiver_verdict_with(&mut ctx, str_obj, owner, "q/Amb", true);
        match verdict {
            Some(Some(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { message },
            )))) => assert_eq!(message, "Cannot cast java.lang.String to q.Amb"),
            other => panic!("expected the owner's cast, got some={}", other.is_some()),
        }
        // Without the authority, the by-name judgement answers (as before).
        assert!(owner_receiver_verdict_with(&mut ctx, str_obj, owner, "q/Amb", false).is_none());
        // A unique name: the by-name cast still judges.
        let base = ctx.ensure_class_initialized("p/Base").expect("p/Base");
        assert!(!owner_refusal_the_name_cannot_judge(
            &ctx, s, base, "p/Base"
        ));
        // A lambda proxy's id is never judged here; nor an unnamed owner.
        assert!(!owner_refusal_the_name_cannot_judge(
            &ctx,
            ClassId::new(0x8000_0001),
            owner,
            "q/Amb"
        ));
        assert!(!owner_refusal_the_name_cannot_judge(&ctx, s, owner, ""));
        // The owner's own instance is admitted by id first.
        let own = ctx.alloc_object(owner, 1);
        assert!(matches!(
            owner_receiver_verdict_with(&mut ctx, own, owner, "q/Amb", true),
            Some(None)
        ));
    }
}

/// Interpreter round i3 wave 79, lane L4: a field accessor `invoke`d at another
/// arity is `asType`'s refusal (page
/// `i70-orch-compatible-bound-getter-invoke-ignores-an-extra-argument`); the
/// end-to-end rows are the last five of
/// `tools/probes/interp/L4/L4W70LinkDerivation.java`.
#[cfg(test)]
mod i79_l4_field_arity_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    #[test]
    fn a_field_accessor_refuses_another_arity() {
        if !mh_strict_exact_signature() {
            return;
        }
        let mut ctx = MockNativeContext::new();
        let bx = ctx.ensure_class_initialized("p/Box").expect("p/Box");
        let recv = ctx.alloc_object(bx, 2);

        // A bound instance getter: `()int`, its `MH_DESC` naming the receiver.
        let get =
            alloc_method_handle(&mut ctx, "p/Box", "v", "(Lp/Box;)I", MH_KIND_GETTER).unwrap();
        ctx.set_field(get, MH_BOUND, Value::Object(Some(recv)));
        assert!(bound_handle_arity_refusal(&mut ctx, get, "(I)I", Some("()I")).is_some());
        assert!(bound_handle_arity_refusal(&mut ctx, get, "()I", Some("()I")).is_none());
        // The same arity is a conversion, not this refusal's business.
        assert!(bound_handle_arity_refusal(&mut ctx, get, "()J", Some("()I")).is_none());
        // A `type` that is not the leaf minus the receiver is never judged.
        assert!(bound_handle_arity_refusal(&mut ctx, get, "(I)I", Some("(Lp/Box;)I")).is_none());
        assert!(bound_handle_arity_refusal(&mut ctx, get, "(I)I", None).is_none());

        // Unbound: the `type` is the `MH_DESC`.
        let uget =
            alloc_method_handle(&mut ctx, "p/Box", "v", "(Lp/Box;)I", MH_KIND_GETTER).unwrap();
        assert!(
            bound_handle_arity_refusal(&mut ctx, uget, "(Lp/Box;I)I", Some("(Lp/Box;)I")).is_some()
        );
        assert!(
            bound_handle_arity_refusal(&mut ctx, uget, "(Lp/Box;)I", Some("(Lp/Box;)I")).is_none()
        );
        assert!(bound_handle_arity_refusal(&mut ctx, uget, "(Lp/Box;I)I", Some("()I")).is_none());

        // A static getter and an instance setter.
        let sget = alloc_method_handle(&mut ctx, "p/Box", "S", "()I", MH_KIND_GETTER).unwrap();
        assert!(bound_handle_arity_refusal(&mut ctx, sget, "(I)I", Some("()I")).is_some());
        let set =
            alloc_method_handle(&mut ctx, "p/Box", "v", "(Lp/Box;I)V", MH_KIND_SETTER).unwrap();
        assert!(
            bound_handle_arity_refusal(&mut ctx, set, "(Lp/Box;)V", Some("(Lp/Box;I)V")).is_some()
        );
        assert!(
            bound_handle_arity_refusal(&mut ctx, set, "(Lp/Box;I)V", Some("(Lp/Box;I)V")).is_none()
        );
    }
}

/// Interpreter round i3 wave 79, lane L4: the synthetic `MethodType` carrier of
/// `methodType(Class, Class[])` keeps a copy of the caller's array (page
/// `i76-L4-synthetic-method-type-keeps-the-callers-parameter-array`). The mock
/// answers `method_exists` false, so the JDK's factory chain declines and the
/// carrier is minted, as in a synthetic class library.
#[cfg(test)]
mod i79_l4_method_type_copy_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    #[test]
    fn the_class_array_overload_keeps_a_copy_on_the_synthetic_carrier() {
        let mut ctx = MockNativeContext::new();
        let s = ctx
            .ensure_class_initialized("java/lang/String")
            .expect("String");
        let i = ctx
            .ensure_class_initialized("java/lang/Integer")
            .expect("Integer");
        let o = ctx
            .ensure_class_initialized("java/lang/Object")
            .expect("Object");
        let (s_m, i_m, o_m) = (
            ctx.get_class_mirror(s),
            ctx.get_class_mirror(i),
            ctx.get_class_mirror(o),
        );
        let arr = new_class_array(&mut ctx, 1);
        ctx.set_array_element(arr, 0, Value::Object(Some(s_m)));
        let Ok(Some(Value::Object(Some(mt)))) = make_method_type_copying(&mut ctx, o_m, Some(arr))
        else {
            panic!("a carrier");
        };
        let Value::Object(Some(kept)) = ctx.get_field(mt, 1) else {
            panic!("the carrier's parameter array");
        };
        assert!(
            kept != arr,
            "the carrier keeps a copy, not the caller's array"
        );
        assert_eq!(ctx.array_length(kept), 1);
        // The caller rewrites its array: the type does not move.
        ctx.set_array_element(arr, 0, Value::Object(Some(i_m)));
        assert_eq!(ctx.get_array_element(kept, 0), Value::Object(Some(s_m)));
        // No array: an empty one, as before.
        let Ok(Some(Value::Object(Some(none)))) = make_method_type_copying(&mut ctx, o_m, None)
        else {
            panic!("a carrier");
        };
        let Value::Object(Some(empty)) = ctx.get_field(none, 1) else {
            panic!("an empty parameter array");
        };
        assert_eq!(ctx.array_length(empty), 0);
    }
}

/// Interpreter round i3 wave 80, lane L4: the flags `MethodHandleNatives.resolve`
/// writes for a lambda proxy's captured field
/// (`mhn_resolve_lambda_proxy_field`; page
/// `i61-L4-lambda-proxy-handles-check-no-receiver-and-do-not-crack`). The
/// end-to-end rows are `tools/probes/interp/L4/L4W61LambdaProxyHandleGaps.java`
/// (`findVarHandle*`) and `tools/probes/interp/L4/L4W61LambdaProxyFieldHandles.java`.
#[cfg(test)]
mod i80_l4_lambda_proxy_field_resolve_tests {
    use super::*;

    const IS_FIELD: i32 = 0x0004_0000;

    #[test]
    fn a_hidden_final_capture_gets_its_access_and_trusted_final() {
        // `getField` request: IS_FIELD | REF_getField << 24, no modifiers.
        let request = IS_FIELD | (1 << 24);
        let flags = mhn_hidden_field_flags(request, 1, 0x0012, true);
        assert_eq!(flags, IS_FIELD | (1 << 24) | 0x0012 | MN_TRUSTED_FINAL);
        // A non-final field of a hidden class: no trusted bit.
        assert_eq!(
            mhn_hidden_field_flags(request, 1, 0x0002, true),
            IS_FIELD | (1 << 24) | 0x0002
        );
        // A final field of an ordinary class: no trusted bit.
        assert_eq!(
            mhn_hidden_field_flags(request, 1, 0x0012, false),
            IS_FIELD | (1 << 24) | 0x0012
        );
        // A `putField` request keeps its kind; stale modifier bits are replaced.
        let put = IS_FIELD | (3 << 24) | 0x0001;
        assert_eq!(
            mhn_hidden_field_flags(put, 3, 0x0012, true),
            IS_FIELD | (3 << 24) | 0x0012 | MN_TRUSTED_FINAL
        );
    }

    #[test]
    fn a_request_of_the_other_static_ness_is_rewritten_to_the_field_s_own() {
        assert_eq!(mhn_field_ref_kind(1, false), 1);
        assert_eq!(mhn_field_ref_kind(3, false), 3);
        assert_eq!(mhn_field_ref_kind(2, false), 1);
        assert_eq!(mhn_field_ref_kind(4, false), 3);
        assert_eq!(mhn_field_ref_kind(1, true), 2);
        assert_eq!(mhn_field_ref_kind(3, true), 4);
        assert_eq!(mhn_field_ref_kind(2, true), 2);
        assert_eq!(mhn_field_ref_kind(4, true), 4);
    }
}

/// Interpreter round i3 wave 80, lane L4: the plan of
/// `CRATONVM_MH_REAL_FIELD_CONVERTING_SERVE` (`mh_field_converting_plan`) and
/// its widening. The end-to-end rows are
/// `tools/probes/interp/L4/L4W80FieldConvertingBench.java`.
#[cfg(test)]
mod i80_l4_field_converting_plan_tests {
    use super::*;

    const BOX: &str = "Lp/Box;";

    fn plan(declared: &str, site: &str, is_static: bool) -> Option<FieldConvertingPlan> {
        mh_field_converting_plan(declared, site, is_static)
    }

    #[test]
    fn the_bench_sites_are_served() {
        let d = format!("({BOX})I");
        assert_eq!(
            plan(&d, &format!("({BOX})Ljava/lang/Object;"), false),
            Some(FieldConvertingPlan {
                receiver_cast: None,
                ret: FieldConvertingReturn::Box
            })
        );
        assert_eq!(
            plan(&d, &format!("({BOX})J"), false),
            Some(FieldConvertingPlan {
                receiver_cast: None,
                ret: FieldConvertingReturn::Widen(b'J')
            })
        );
        assert_eq!(
            plan("()I", "()Ljava/lang/Object;", true),
            Some(FieldConvertingPlan {
                receiver_cast: None,
                ret: FieldConvertingReturn::Box
            })
        );
    }

    #[test]
    fn a_wider_receiver_parameter_is_a_cast_to_the_declared_class() {
        let d = format!("({BOX})I");
        assert_eq!(
            plan(&d, "(Ljava/lang/Object;)I", false),
            Some(FieldConvertingPlan {
                receiver_cast: Some(BOX.to_string()),
                ret: FieldConvertingReturn::Same
            })
        );
        assert_eq!(
            plan(&d, "(Ljava/lang/Object;)Ljava/lang/Number;", false),
            Some(FieldConvertingPlan {
                receiver_cast: Some(BOX.to_string()),
                ret: FieldConvertingReturn::Box
            })
        );
        // A primitive receiver parameter is not a cast.
        assert_eq!(plan(&d, "(I)I", false), None);
    }

    #[test]
    fn everything_else_declines() {
        let d = format!("({BOX})I");
        // Exact (the other lanes'), void, narrowing, unboxing-shaped, arity.
        assert_eq!(plan(&d, &d, false), None);
        assert_eq!(plan(&d, &format!("({BOX})V"), false), None);
        assert_eq!(plan(&d, &format!("({BOX})S"), false), None);
        assert_eq!(plan(&d, &format!("({BOX})Ljava/lang/String;"), false), None);
        assert_eq!(plan(&d, &format!("({BOX})Ljava/lang/Long;"), false), None);
        assert_eq!(plan(&d, "()I", false), None);
        assert_eq!(plan(&d, &format!("({BOX}I)I"), false), None);
        // `boolean` / `char` are no `Number`; `boolean` widens to nothing.
        assert_eq!(plan("()Z", "()Ljava/lang/Number;", true), None);
        assert_eq!(plan("()C", "()Ljava/lang/Number;", true), None);
        assert_eq!(plan("()Z", "()I", true), None);
        // A reference field: only an `Object` site.
        assert_eq!(
            plan("()Ljava/lang/String;", "()Ljava/lang/Object;", true),
            Some(FieldConvertingPlan {
                receiver_cast: None,
                ret: FieldConvertingReturn::Same
            })
        );
        assert_eq!(
            plan("()Ljava/lang/String;", "()Ljava/lang/CharSequence;", true),
            None
        );
        assert_eq!(plan("()Ljava/lang/Integer;", "()I", true), None);
    }

    #[test]
    fn widening_follows_jls_5_1_2() {
        for (from, tos) in [
            (b'B', &b"SIJFD"[..]),
            (b'S', &b"IJFD"[..]),
            (b'C', &b"IJFD"[..]),
            (b'I', &b"JFD"[..]),
            (b'J', &b"FD"[..]),
            (b'F', &b"D"[..]),
        ] {
            for to in *b"ZBSCIJFD" {
                assert_eq!(
                    field_converting_widens(from, to),
                    tos.contains(&to),
                    "{} -> {}",
                    from as char,
                    to as char
                );
            }
        }
        for to in *b"ZBSCIJFD" {
            assert!(!field_converting_widens(b'Z', to));
            assert!(!field_converting_widens(b'D', to));
        }
        assert!(matches!(
            field_converting_widen(Value::Int(-7), b'J'),
            Value::Long(-7)
        ));
        assert!(
            matches!(field_converting_widen(Value::Int(3), b'D'), Value::Double(d) if d == 3.0)
        );
        assert!(
            matches!(field_converting_widen(Value::Int(16_777_217), b'F'), Value::Float(f) if f == 16_777_216.0)
        );
        assert!(
            matches!(field_converting_widen(Value::Long(1 << 40), b'D'), Value::Double(d) if d == (1u64 << 40) as f64)
        );
        assert!(
            matches!(field_converting_widen(Value::Float(1.5), b'D'), Value::Double(d) if d == 1.5)
        );
        assert!(matches!(
            field_converting_widen(Value::Int(9), b'I'),
            Value::Int(9)
        ));
    }
}

/// Interpreter round i3 wave 81, lane L4: [`mh_site_type_link_error`] rewrites
/// only a thrown `TypeNotPresentException`; the end-to-end rows are
/// `tools/probes/interp/L4/L4W81SiteTypeMissingClass.java` (an application
/// caller) and `tools/probes/interp/L4/L4W79BootSiteType.java` (a boot caller,
/// with `CRATONVM_MH_SITE_TYPE_BOOT_LOADER=1`).
#[cfg(test)]
mod i81_l4_site_type_link_error_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;

    #[test]
    fn a_vm_internal_failure_passes_through_unchanged() {
        let mut ctx = MockNativeContext::new();
        for boot_caller in [false, true] {
            let failed = MethodCallFailed::InternalError(VmError::Internal {
                message: "site type".to_string(),
            });
            match mh_site_type_link_error(&mut ctx, failed, boot_caller) {
                MethodCallFailed::InternalError(VmError::Internal { message }) => {
                    assert_eq!(message, "site type", "boot_caller={boot_caller}");
                }
                other => panic!("boot_caller={boot_caller}: rewritten to {other:?}"),
            }
        }
    }
}

/// Interpreter round i3 wave 81, lane L4: the method-member resolve memo
/// (`CRATONVM_MHN_RESOLVE_METHOD_MEMO`, default off). Every switch is set
/// explicitly through a thread override. The end-to-end rows are
/// `tools/probes/interp/L4/L4W81MethodResolveMemo.java` (behaviour) and
/// `tools/probes/interp/L4/L4W81MethodFinderBench.java`.
#[cfg(test)]
mod i81_l4_method_resolve_memo_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::flags::with_thread_overrides;

    #[test]
    fn the_switch_is_off_unless_a_truthy_word() {
        // Explicit words only: the default (off) is not pinned here.
        for (word, on) in [("0", false), ("off", false), ("1", true), ("true", true)] {
            with_thread_overrides(&[("CRATONVM_MHN_RESOLVE_METHOD_MEMO", Some(word))], || {
                assert_eq!(mhn_resolve_method_memo_enabled(), on, "{word:?}");
            });
        }
    }

    fn key() -> MhnMethodKey {
        MhnMethodKey {
            objects: [0x1000, 0x2000, 0x3000],
            flags: 0x0501_0000,
            caller: 0x4000,
            mode: 0x3f,
            speculative: false,
        }
    }

    #[test]
    fn the_memo_answers_only_its_own_member_caller_mode_and_vm() {
        let ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x8101);
        let epoch = ctx.gc_collection_count();
        let k = key();
        mhn_method_memo_put(&ctx, epoch, k, Some(0x0501_0002));
        assert_eq!(mhn_method_memo_get(&ctx, &k), Some(Some(0x0501_0002)));
        for (other, why) in [
            (
                MhnMethodKey {
                    caller: 0x4008,
                    ..k
                },
                "another caller",
            ),
            (MhnMethodKey { caller: 0, ..k }, "a null caller"),
            (MhnMethodKey { mode: 0x1f, ..k }, "another lookup mode"),
            (MhnMethodKey { mode: -1, ..k }, "the trusted mode"),
            (
                MhnMethodKey {
                    speculative: true,
                    ..k
                },
                "a speculative resolve",
            ),
            (
                MhnMethodKey {
                    flags: 0x0601_0000,
                    ..k
                },
                "another reference kind",
            ),
            (
                MhnMethodKey {
                    objects: [0x1000, 0x2008, 0x3000],
                    ..k
                },
                "another name",
            ),
            (
                MhnMethodKey {
                    objects: [0x1000, 0x2000, 0x3008],
                    ..k
                },
                "another type",
            ),
            (
                MhnMethodKey {
                    objects: [0x1008, 0x2000, 0x3000],
                    ..k
                },
                "another class",
            ),
        ] {
            assert_ne!(other, k, "{why}");
            assert_eq!(mhn_method_memo_get(&ctx, &other), None, "{why}");
        }
        // A found member that wrote no flags is remembered as such.
        let k2 = MhnMethodKey {
            caller: 0x5000,
            ..k
        };
        mhn_method_memo_put(&ctx, epoch, k2, None);
        assert_eq!(mhn_method_memo_get(&ctx, &k2), Some(None));
        let other_vm = MockNativeContext::new();
        other_vm.set_vm_identity(0x8102);
        assert_eq!(mhn_method_memo_get(&other_vm, &k), None, "another VM");
    }

    #[test]
    fn nothing_is_stored_across_a_collection_or_for_vm_zero() {
        let ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x8103);
        let k = MhnMethodKey {
            caller: 0x6000,
            ..key()
        };
        let stale_epoch = ctx.gc_collection_count().wrapping_add(1);
        mhn_method_memo_put(&ctx, stale_epoch, k, Some(1));
        assert_eq!(mhn_method_memo_get(&ctx, &k), None);
        let mock = MockNativeContext::new();
        let epoch = mock.gc_collection_count();
        mhn_method_memo_put(&mock, epoch, k, Some(1));
        assert_eq!(mhn_method_memo_get(&mock, &k), None, "vm 0");
    }

    #[test]
    fn every_method_memo_slot_is_in_range() {
        for a in [0usize, 8, 0x1234_5678, usize::MAX] {
            for (mode, speculative) in [(0, false), (-1, true), (0x3f, false)] {
                let k = MhnMethodKey {
                    objects: [a, a.wrapping_mul(3), a ^ 0x55],
                    flags: i32::MIN,
                    caller: a.rotate_left(5),
                    mode,
                    speculative,
                };
                assert!(mhn_method_memo_slot(&k) < MHN_METHOD_MEMO_SLOTS);
            }
        }
    }

    /// `CRATONVM_MH_COMPAT_FINDER_NEST_ACCESS` (wave 81 rework): a kill
    /// switch; explicit words only (the default is not pinned).
    #[test]
    fn the_compat_finder_nest_access_switch_reads_its_words() {
        for (word, on) in [("0", false), ("off", false), ("1", true), ("true", true)] {
            with_thread_overrides(
                &[("CRATONVM_MH_COMPAT_FINDER_NEST_ACCESS", Some(word))],
                || {
                    assert_eq!(
                        crate::nbflags().mh_compat_finder_nest_access,
                        on,
                        "{word:?}"
                    );
                },
            );
        }
    }
}

#[cfg(test)]
mod i61_method_type_identity_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    #[test]
    fn adaptation_identity_requires_the_same_class_mirrors() {
        let mut ctx = MockNativeContext::new();
        let mt_class = ctx
            .ensure_class_initialized("java/lang/invoke/MethodType")
            .unwrap();
        let mirror_class = ctx.ensure_class_initialized("java/lang/Class").unwrap();
        // Anonymous/hidden mirrors have no renderable descriptor. Distinct
        // mirrors must still force adaptation, as must a changed return class.
        let first = ctx.alloc_object(mirror_class, 4);
        let second = ctx.alloc_object(mirror_class, 4);
        // `MethodType.ptypes` is a `Class[]` (wave 96: was an untyped `Object[]`).
        let make = |ctx: &mut MockNativeContext, ret, param| {
            let params = ctx.new_ref_array(mirror_class, 1);
            ctx.set_array_element(params, 0, Value::Object(Some(param)));
            let mt = ctx.alloc_object(mt_class, 6);
            ctx.set_field_by_name(mt, "rtype", Value::Object(Some(ret)));
            ctx.set_field_by_name(mt, "ptypes", Value::Object(Some(params)));
            mt
        };
        let a = make(&mut ctx, first, first);
        let b = make(&mut ctx, first, first);
        let other_param = make(&mut ctx, first, second);
        let other_return = make(&mut ctx, second, first);
        assert!(method_types_same_classes(&ctx, a, a));
        assert!(method_types_same_classes(&ctx, a, b));
        assert!(!method_types_same_classes(&ctx, a, other_param));
        assert!(!method_types_same_classes(&ctx, a, other_return));
    }
}

#[cfg(test)]
mod i94_l4_field_link_first_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;

    /// `CRATONVM_MH_FIELD_LINK_FIRST`'s early serve declines, with nothing
    /// done, for a handle that is not a linked field accessor (here a fresh
    /// static method handle) at either door, and for a missing site.
    #[test]
    fn field_link_first_declines_a_handle_that_is_not_a_linked_accessor() {
        let mut ctx = MockNativeContext::new();
        let h = alloc_method_handle(&mut ctx, "C", "m", "(I)I", MH_KIND_STATIC).unwrap();
        for door in [LinkingDoor::InvokeExact, LinkingDoor::Invoke] {
            assert!(
                mh_field_link_first(&mut ctx, h, &[Value::Int(1)], Some("(I)I"), door).is_none()
            );
            assert!(mh_field_link_first(&mut ctx, h, &[Value::Int(1)], None, door).is_none());
        }
    }
}

#[cfg(test)]
mod jr21_reflect2_linker_key_tests {
    use super::*;

    /// `CRATONVM_JIT_MH_COMPILED_LINK_LEAN`'s one-`String` key is the key
    /// [`mh_linked_invoker`] files the row under, byte for byte.
    #[test]
    fn the_lean_key_is_the_door_key() {
        for site in [
            "()Ljava/lang/Object;",
            "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            "(II)I",
            "",
        ] {
            for caller in [1u32, 7, 4096, u32::MAX] {
                assert_eq!(
                    jr21_reflect2_linker_key(site, caller),
                    cratonvm_native_api::method_type_table::class_keyed(
                        &format!("linker:invokeExact:{site}"),
                        &[caller],
                    ),
                    "{site} {caller}"
                );
            }
        }
    }
}

#[cfg(test)]
mod i95_l4_linker_basic_and_field_lane_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;

    /// The basic type the JDK linker's cached form is keyed by: references
    /// and arrays erase to `Object`, subword primitives to `int` (the return
    /// too), `long` / `double` take two slots, and a site past 253 slots (a
    /// customized, uncached linker) has no key.
    #[test]
    fn the_linker_basic_descriptor_erases_as_method_type_basic_type() {
        assert_eq!(
            linker_basic_descriptor("(LBox;)I").as_deref(),
            Some("(Ljava/lang/Object;)I")
        );
        assert_eq!(
            linker_basic_descriptor("(LBox;ZBSC[I[[LBox;)Z").as_deref(),
            Some("(Ljava/lang/Object;IIIILjava/lang/Object;Ljava/lang/Object;)I")
        );
        assert_eq!(linker_basic_descriptor("(JD)V").as_deref(), Some("(JD)V"));
        assert_eq!(
            linker_basic_descriptor("()Ljava/lang/String;").as_deref(),
            Some("()Ljava/lang/Object;")
        );
        let wide = "J".repeat(127);
        assert!(linker_basic_descriptor(&format!("({wide})V")).is_none());
        let fits = "J".repeat(126);
        assert!(linker_basic_descriptor(&format!("({fits}I)V")).is_some());
        assert!(linker_basic_descriptor(&format!("({fits}II)V")).is_none());
        assert!(linker_basic_descriptor("(V)V").is_none());
        assert!(linker_basic_descriptor("not a descriptor").is_none());
    }

    /// `CRATONVM_MH_REAL_FIELD_BEFORE_LINKER` never serves a synthetic
    /// carrier (the linker never runs for one either) or a missing site.
    #[test]
    fn the_field_lane_declines_a_carrier_and_a_missing_site() {
        let mut ctx = MockNativeContext::new();
        let h = alloc_method_handle(&mut ctx, "C", "m", "(I)I", MH_KIND_STATIC).unwrap();
        let one = [Value::Int(1)];
        assert!(mh_real_field_before_linker(&mut ctx, h, &one, Some("(I)I"), 0).is_none());
        assert!(mh_real_field_before_linker(&mut ctx, h, &one, None, 0).is_none());
    }

    /// `CRATONVM_MH_REAL_DIRECT_BEFORE_LINKER` (interpreter round i5 wave 102,
    /// lane L4) declines the same: a synthetic carrier, a missing site.
    #[test]
    fn the_direct_lane_declines_a_carrier_and_a_missing_site() {
        let mut ctx = MockNativeContext::new();
        let h = alloc_method_handle(&mut ctx, "C", "m", "(I)I", MH_KIND_STATIC).unwrap();
        let one = [Value::Int(1)];
        assert!(mh_real_direct_before_linker(&mut ctx, h, &one, Some("(I)I"), 0).is_none());
        assert!(mh_real_direct_before_linker(&mut ctx, h, &one, None, 0).is_none());
    }
}

#[cfg(test)]
mod i101_l4_bound_field_lane_tests {
    use super::bound_site_matches;

    /// The bound handle's site is the accessor's type with its first parameter
    /// dropped, whatever that parameter's spelling (class, array, primitive).
    #[test]
    fn i101_bound_site_drops_exactly_the_first_parameter() {
        assert!(bound_site_matches("(LBox;)I", "()I"));
        assert!(bound_site_matches("(LBox;I)V", "(I)V"));
        assert!(bound_site_matches("(Lp/Box;Ljava/lang/String;)V", "(Ljava/lang/String;)V"));
        assert!(bound_site_matches("([[LBox;)J", "()J"));
        assert!(bound_site_matches("(IJ)V", "(J)V"));
        assert!(!bound_site_matches("(LBox;)I", "(LBox;)I"));
        assert!(!bound_site_matches("(LBox;)I", "()J"));
        assert!(!bound_site_matches("()I", "()I"));
        assert!(!bound_site_matches("(LBox)I", "()I"));
        assert!(!bound_site_matches("LBox;", "()I"));
    }

    /// Wave 102: two handles in ONE set, called alternately, both stay
    /// recorded (the direct-mapped memo evicted one per call).
    #[test]
    fn i102_two_handles_sharing_a_set_keep_their_lines() {
        use super::{
            bound_field_memo_file, bound_field_memo_find, bound_field_memo_set, BoundFieldLine,
            BOUND_FIELD_MEMO_WAYS,
        };
        let vm = 0x5eed_usize;
        let a = 0x1000_usize;
        let set = bound_field_memo_set(a);
        let mut same: Vec<usize> = (1..100_000usize)
            .map(|k| a + k * 8)
            .filter(|&x| bound_field_memo_set(x) == set)
            .take(BOUND_FIELD_MEMO_WAYS)
            .collect();
        assert_eq!(same.len(), BOUND_FIELD_MEMO_WAYS);
        let b = same.remove(0);
        let line = |addr| BoundFieldLine {
            vm,
            epoch: 1,
            addr,
            accessor: None,
            base: None,
        };
        bound_field_memo_file(line(a));
        bound_field_memo_file(line(b));
        for _ in 0..8 {
            assert!(bound_field_memo_find(vm, 1, a).is_some());
            assert!(bound_field_memo_find(vm, 1, b).is_some());
        }
        // Another epoch is a miss; filing the same handle again replaces its
        // line instead of taking a second way.
        assert!(bound_field_memo_find(vm, 2, a).is_none());
        bound_field_memo_file(BoundFieldLine { epoch: 2, ..line(a) });
        for &x in &same {
            bound_field_memo_file(line(x));
        }
        // Three newer handles filled the set: `a` (newest of the first two
        // after its refile) is still in, `b` is the evicted LRU line.
        assert!(bound_field_memo_find(vm, 2, a).is_some());
        assert!(bound_field_memo_find(vm, 1, b).is_none());
    }
}

/// gci cl48 (page
/// `gci-i47w-jw1-compatible-child-first-loader-statics-read-back-wrong-FIXED-20261017`):
/// a real `DirectMethodHandle$StaticAccessor` resolves its `staticOffset`
/// against its own `staticBase`, so the handles of the child-first copies of
/// one class (one minted offset) each reach their own copy's static.
#[cfg(test)]
mod gci_i48_cl48_static_accessor_tests {
    use super::real_static_accessor_target;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        FieldMetadata, NativeClassAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess,
    };
    use cratonvm_types::{ObjectRef, Value};

    // An identity no other test uses (mock contexts default to 0).
    const VM: usize = 0xC148_0011;
    const NAME: &str = "gci48cl48/AccessorHolder";

    /// A `StaticAccessor` stand-in whose only field is `staticBase`.
    fn accessor(ctx: &mut MockNativeContext, base: Option<ObjectRef>) -> ObjectRef {
        let cid = ctx
            .ensure_class_initialized("java/lang/invoke/DirectMethodHandle$StaticAccessor")
            .expect("mock class id");
        ctx.set_declared_fields(
            cid,
            vec![FieldMetadata {
                name: "staticBase".to_string(),
                descriptor: "Ljava/lang/Object;".to_string(),
                access_flags: 0x0010,
                slot_index: 0,
                declaring_class_id: cid,
                is_static: false,
            }],
        );
        let acc = ctx.alloc_object(cid, 1);
        ctx.set_field(acc, 0, Value::Object(base));
        acc
    }

    #[test]
    fn gci_i48_cl48_each_static_accessor_reaches_its_own_copy() {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        let mut classes = vec![ctx.ensure_class_initialized(NAME).expect("mock class id")];
        for _ in 1..8 {
            classes.push(ctx.declare_second_copy(NAME));
        }
        let offsets: Vec<i64> = classes
            .iter()
            .map(|&c| crate::register_static_field_offset_in_vm(VM, NAME, "graph", c, 0))
            .collect();
        // Since dev's `eab6d094dd` a static offset carries the class id: every
        // copy has its own offset, and each accessor names its own copy.
        let mut distinct = offsets.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), offsets.len(), "one copy, one offset");
        for (k, &c) in classes.iter().enumerate() {
            let offset = offsets[k] as usize;
            let mirror = ctx.get_class_mirror(c);
            let acc = accessor(&mut ctx, Some(mirror));
            assert_eq!(
                real_static_accessor_target(&mut ctx, acc, offset),
                Some((c, 0)),
                "the accessor of copy {} names that copy",
                c.as_u32()
            );
            // No base: the copy registered under that offset.
            let acc = accessor(&mut ctx, None);
            assert_eq!(real_static_accessor_target(&mut ctx, acc, offset), Some((c, 0)));
        }
        crate::forget_vm_unsafe_side_stores(VM);
    }
}

/// Interpreter round i6 wave 121, lane L2 (page
/// `i115-L4-proposal-shadowed-jdk-bodies-compile-for-their-bytecode-only-reentries`):
/// the re-entry-only set moved from `vm_init.rs` to its registrar. The four
/// rows are named, each still an `Intrinsic` (marking changes no kind), and
/// a second registration pass adds no duplicate.
#[cfg(test)]
mod i121_l2_reentry_only_bodies_tests {
    use super::register_t4_method_handle_invoke;
    use cratonvm_native_api::{NativeKind, NativeMethodRegistry};

    #[test]
    fn t4_registrar_names_the_four_reentry_only_bodies() {
        let mut registry = NativeMethodRegistry::new();
        register_t4_method_handle_invoke(&mut registry);
        register_t4_method_handle_invoke(&mut registry);
        let mh = "java/lang/invoke/MethodHandle";
        let want = [
            (
                mh,
                "asType",
                "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
            ),
            (mh, "invokeWithArguments", "([Ljava/lang/Object;)Ljava/lang/Object;"),
            (mh, "invokeWithArguments", "(Ljava/util/List;)Ljava/lang/Object;"),
            (mh, "bindTo", "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;"),
        ];
        assert_eq!(registry.reentry_only_bodies(), &want[..]);
        for (c, m, d) in want {
            assert_eq!(
                registry.find_with_kind(c, m, d).map(|(_, kind)| kind),
                Some(NativeKind::Intrinsic),
                "{c}.{m}{d} stays an Intrinsic"
            );
        }
    }
}

/// JIT round 21 wave 10, lane indy: `MHN_resolve`'s refused method access
/// spells HotSpot 25's `LinkResolver::check_method_accessability` text.
#[cfg(test)]
mod r21_w10_mhn_access_text_tests {
    use super::*;

    #[test]
    fn external_method_names_follow_print_external_name() {
        assert_eq!(
            mhn_external_method_name("p.D", "m", "()V").as_deref(),
            Some("void p.D.m()")
        );
        assert_eq!(
            mhn_external_method_name("p.D", "<init>", "(ILjava/lang/String;)V").as_deref(),
            Some("void p.D.<init>(int, java.lang.String)")
        );
        assert_eq!(
            mhn_external_method_name("p.D", "f", "([[J[Lq/E;Z)[Ljava/lang/Object;").as_deref(),
            Some("java.lang.Object[] p.D.f(long[][], q.E[], boolean)")
        );
        assert_eq!(
            mhn_external_method_name("p.D", "m", "(V)V").as_deref(),
            Some("void p.D.m(void)")
        );
        assert_eq!(mhn_external_method_name("p.D", "m", "()"), None);
        assert_eq!(mhn_external_method_name("p.D", "m", "(Lp/X)V"), None);
        assert_eq!(mhn_external_method_name("p.D", "m", "()[V"), None);
    }

    #[test]
    fn private_method_text_with_joint_unnamed_module_clause() {
        let clause = mhn_member_module_clause(
            ("q.C", "unnamed module", "'app'"),
            ("p.D", "unnamed module", "'app'"),
            true,
        );
        assert_eq!(
            mhn_method_access_denied_text("q.C", MHN_ACC_PRIVATE, "void p.D.m()", Some(&clause)),
            "class q.C tried to access private method 'void p.D.m()' \
             (q.C and p.D are in unnamed module of loader 'app')"
        );
    }

    #[test]
    fn package_and_protected_texts_and_two_module_clause() {
        let clause = mhn_member_module_clause(
            ("q.C", "unnamed module", "'app'"),
            ("java.lang.Thing", "module java.base", "'bootstrap'"),
            false,
        );
        assert_eq!(
            mhn_method_access_denied_text("q.C", 0, "int java.lang.Thing.m()", Some(&clause)),
            "class q.C tried to access method 'int java.lang.Thing.m()' \
             (q.C is in unnamed module of loader 'app'; java.lang.Thing is in module \
             java.base of loader 'bootstrap')"
        );
        // abstract, protected, private: HotSpot's order.
        assert_eq!(
            mhn_method_access_denied_text("q.C", 0x0004 | MHN_ACC_ABSTRACT, "void p.D.m()", None),
            "class q.C tried to access abstract protected method 'void p.D.m()'"
        );
        assert_eq!(
            mhn_method_access_denied_text(
                "q.C",
                MHN_ACC_PRIVATE | MHN_ACC_STATIC,
                "void p.D.<init>()",
                None
            ),
            "class q.C tried to access private method 'void p.D.<init>()'"
        );
    }
}

/// JIT round 22 wave 3, lane runtime (`CRATONVM_MH_LANES_KEEP_CUSTOMIZATION`,
/// page `r22-ctor-c0-hidden-frame-names-the-holder-not-the-customized-form-20261020`):
/// the before-linker lanes' `checkCustomized` count -- raised below the
/// threshold, the due call declined to the linker, a customized form left
/// alone, the take-back of a declined lane, and `=0` the old (no count) answer.
#[cfg(test)]
mod r22w3_runtime_lanes_keep_customization_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeSystemAccess};
    use cratonvm_types::flags::with_thread_overrides;

    fn handle(ctx: &mut MockNativeContext) -> (ObjectRef, ObjectRef) {
        let field = |name: &str, desc: &str, slot: usize, cid: ClassId| {
            cratonvm_native_api::FieldMetadata {
                name: name.to_string(),
                descriptor: desc.to_string(),
                access_flags: 0,
                slot_index: slot,
                declaring_class_id: cid,
                is_static: false,
            }
        };
        let (mh_cid, form_cid) = (ClassId::new(0x7201), ClassId::new(0x7202));
        ctx.set_declared_fields(
            mh_cid,
            vec![
                field("form", "Ljava/lang/invoke/LambdaForm;", 0, mh_cid),
                field("customizationCount", "B", 1, mh_cid),
            ],
        );
        ctx.set_declared_fields(
            form_cid,
            vec![field("customized", "Ljava/lang/invoke/MethodHandle;", 0, form_cid)],
        );
        let mh = ctx.alloc_object(mh_cid, 2);
        let form = ctx.alloc_object(form_cid, 1);
        ctx.set_field_by_name(mh, "form", Value::Object(Some(form)));
        ctx.set_field_by_name(form, "customized", Value::Object(None));
        ctx.set_field_by_name(mh, "customizationCount", Value::Int(0));
        (mh, form)
    }

    fn count(ctx: &MockNativeContext, mh: ObjectRef) -> Value {
        ctx.get_field_by_name(mh, "customizationCount")
    }

    #[test]
    fn r22w3_a_lane_counts_until_due_then_declines_to_the_linker() {
        let mut ctx = MockNativeContext::new();
        let (mh, form) = handle(&mut ctx);
        with_thread_overrides(&[("CRATONVM_MH_LANES_KEEP_CUSTOMIZATION", None)], || {
            for k in 0..MH_CUSTOMIZE_THRESHOLD {
                assert_eq!(lane_check_customized(&ctx, mh), Some(Some(k)));
                assert_eq!(count(&ctx, mh), Value::Int(k + 1));
            }
            // The 128th call is due: the lane declines, the count stays 127.
            assert_eq!(lane_check_customized(&ctx, mh), None);
            assert_eq!(count(&ctx, mh), Value::Int(MH_CUSTOMIZE_THRESHOLD));
            // The linker customized the form: the lane serves, counting nothing.
            ctx.set_field_by_name(form, "customized", Value::Object(Some(mh)));
            assert_eq!(lane_check_customized(&ctx, mh), Some(None));
            assert_eq!(count(&ctx, mh), Value::Int(MH_CUSTOMIZE_THRESHOLD));
        });
    }

    #[test]
    fn r22w3_a_declined_lane_takes_its_count_back() {
        let mut ctx = MockNativeContext::new();
        let (mh, _) = handle(&mut ctx);
        ctx.set_field_by_name(mh, "customizationCount", Value::Int(5));
        with_thread_overrides(&[("CRATONVM_MH_LANES_KEEP_CUSTOMIZATION", None)], || {
            let counted = lane_check_customized(&ctx, mh).expect("not due");
            assert_eq!(count(&ctx, mh), Value::Int(6));
            lane_uncount_customization(&mut ctx, mh, counted);
            assert_eq!(count(&ctx, mh), Value::Int(5));
            // Another thread counted on top of ours: the take-back leaves it.
            let counted = lane_check_customized(&ctx, mh).expect("not due");
            ctx.set_field_by_name(mh, "customizationCount", Value::Int(7));
            lane_uncount_customization(&mut ctx, mh, counted);
            assert_eq!(count(&ctx, mh), Value::Int(7));
        });
    }

    #[test]
    fn r22w3_the_kill_switch_restores_the_uncounted_lanes() {
        let mut ctx = MockNativeContext::new();
        let (mh, _) = handle(&mut ctx);
        ctx.set_field_by_name(mh, "customizationCount", Value::Int(MH_CUSTOMIZE_THRESHOLD));
        with_thread_overrides(&[("CRATONVM_MH_LANES_KEEP_CUSTOMIZATION", Some("0"))], || {
            assert_eq!(lane_check_customized(&ctx, mh), Some(None));
            assert_eq!(count(&ctx, mh), Value::Int(MH_CUSTOMIZE_THRESHOLD));
        });
    }

    /// The per-VM slot path (wave-3 tightening): resolved once, then the
    /// count is read and written by slot, and a customized handle is answered
    /// from the form alone.
    #[test]
    fn r22w3_the_slot_path_counts_and_answers_a_customized_handle() {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x7222_0003);
        let mh_cid = ctx.ensure_class_initialized("java/lang/invoke/MethodHandle").expect("mh");
        let lf_cid = ctx.ensure_class_initialized("java/lang/invoke/LambdaForm").expect("lf");
        let field = |name: &str, slot: usize, cid: ClassId| cratonvm_native_api::FieldMetadata {
            name: name.to_string(),
            descriptor: "Ljava/lang/Object;".to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            mh_cid,
            vec![field("type", 0, mh_cid), field("form", 1, mh_cid), field("customizationCount", 4, mh_cid)],
        );
        ctx.set_declared_fields(lf_cid, vec![field("customized", 2, lf_cid)]);
        let mh = ctx.alloc_object(mh_cid, 6);
        let form = ctx.alloc_object(lf_cid, 3);
        ctx.set_field(mh, 1, Value::Object(Some(form)));
        ctx.set_field(form, 2, Value::Object(None));
        ctx.set_field(mh, 4, Value::Int(3));
        with_thread_overrides(&[("CRATONVM_MH_LANES_KEEP_CUSTOMIZATION", None)], || {
            assert_eq!(
                lane_customize_slots(&ctx, mh),
                Some(LaneCustomizeSlots { form: 1, count: 4, customized: 2 })
            );
            assert_eq!(lane_check_customized(&ctx, mh), Some(Some(3)));
            assert_eq!(ctx.get_field(mh, 4), Value::Int(4));
            ctx.set_field(form, 2, Value::Object(Some(mh)));
            assert_eq!(lane_check_customized(&ctx, mh), Some(None));
            assert_eq!(ctx.get_field(mh, 4), Value::Int(4));
            // Wave 4 (`lane_due_once`): an uncustomized handle at the threshold
            // declines to the linker once, then is served uncounted.
            ctx.set_field(form, 2, Value::Object(None));
            ctx.set_field(mh, 4, Value::Int(MH_CUSTOMIZE_THRESHOLD));
            assert_eq!(lane_check_customized(&ctx, mh), None, "the due call is the linker's");
            // Build w4 regression: a second ask in the SAME call (the compiled
            // bind declines, the door's lane asks again) still declines; only a
            // linker run files the handle.
            assert_eq!(lane_check_customized(&ctx, mh), None, "asking again is not a linker run");
            lane_note_due_linker_ran(&ctx, mh);
            assert_eq!(lane_check_customized(&ctx, mh), Some(None), "never every call");
            assert_eq!(ctx.get_field(mh, 4), Value::Int(MH_CUSTOMIZE_THRESHOLD));
            // A customized handle is never filed.
            let (mh2, form2) = (ctx.alloc_object(mh_cid, 6), ctx.alloc_object(lf_cid, 3));
            ctx.set_field(mh2, 1, Value::Object(Some(form2)));
            ctx.set_field(form2, 2, Value::Object(Some(mh2)));
            ctx.set_field(mh2, 4, Value::Int(MH_CUSTOMIZE_THRESHOLD));
            lane_note_due_linker_ran(&ctx, mh2);
            ctx.set_field(form2, 2, Value::Object(None));
            assert_eq!(lane_check_customized(&ctx, mh2), None);
        });
    }

    /// Every before-linker lane asks the count (a lane added later without it
    /// is the defect this page records).
    #[test]
    fn r22w3_every_before_linker_lane_asks_the_count() {
        let src: &'static str = LANG_INVOKE_SRC;
        for f in [
            "fn ctor_lane_counted_instantiate(",
            "fn mh_real_direct_before_linker(",
            "fn mh_real_field_before_linker(",
            "fn real_bound_field_serve(",
            "pub fn jit_real_field_lane_warm(",
            // Wave 4: the converting `invoke` lane counts the `asTypeCache` adapter.
            "fn mh_real_field_converting_serve(",
        ] {
            let at = src.find(f).expect(f);
            let end = src[at..].find("\n}\n").expect("fn end");
            assert!(
                src[at..at + end].contains("lane_check_customized("),
                "{f} serves without checkCustomized's count"
            );
        }
        let at = src.find("fn mh_real_constructor_fast(").expect("ctor lane");
        let end = src[at..].find("\n}\n").expect("fn end");
        let body = &src[at..at + end];
        assert!(!body.contains("ctor_lane_instantiate(ctx"), "the ctor lane counts first");
        assert!(body.contains("hidden_frames_never_observed()"));
    }
}

/// Interpreter round i7 wave 135, lane L4 (page i110-L4, "Wave 135"): the two
/// invoker-check `Intrinsic`s read by the per-VM slots, and answer what the
/// by-name reads answered (count raised below the threshold, a customized
/// form left alone, the identical type accepted).
#[cfg(test)]
mod i135_l4_invoker_checks_slot_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeSystemAccess};

    #[test]
    fn i135_l4_check_customized_and_exact_type_by_slot() {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x7135_0004);
        let mh_cid = ctx.ensure_class_initialized("java/lang/invoke/MethodHandle").expect("mh");
        let lf_cid = ctx.ensure_class_initialized("java/lang/invoke/LambdaForm").expect("lf");
        let field = |name: &str, slot: usize, cid: ClassId| cratonvm_native_api::FieldMetadata {
            name: name.to_string(),
            descriptor: "Ljava/lang/Object;".to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            mh_cid,
            vec![field("type", 0, mh_cid), field("form", 1, mh_cid), field("customizationCount", 4, mh_cid)],
        );
        ctx.set_declared_fields(lf_cid, vec![field("customized", 2, lf_cid)]);
        let mh = ctx.alloc_object(mh_cid, 6);
        let form = ctx.alloc_object(lf_cid, 3);
        let mt = ctx.alloc_object(lf_cid, 3);
        ctx.set_field(mh, 0, Value::Object(Some(mt)));
        ctx.set_field(mh, 1, Value::Object(Some(form)));
        ctx.set_field(form, 2, Value::Object(None));
        ctx.set_field(mh, 4, Value::Int(5));
        assert_eq!(invokers_mh_type_slot(&ctx, mh), Some(0));
        let r = invokers_check_customized_intrinsic(&mut ctx, &[Value::Object(Some(mh))]);
        assert!(matches!(r, Ok(None)));
        assert_eq!(ctx.get_field(mh, 4), Value::Int(6), "counted by slot");
        ctx.set_field(form, 2, Value::Object(Some(mh)));
        let r = invokers_check_customized_intrinsic(&mut ctx, &[Value::Object(Some(mh))]);
        assert!(matches!(r, Ok(None)));
        assert_eq!(ctx.get_field(mh, 4), Value::Int(6), "a customized form counts nothing");
        let r = invokers_check_exact_type_intrinsic(
            &mut ctx,
            &[Value::Object(Some(mh)), Value::Object(Some(mt))],
        );
        assert!(matches!(r, Ok(None)), "the identical type is accepted by slot");
    }
}

/// Interpreter round i7 wave 137, lane L4 (`CRATONVM_MH_LF_LEAF_INTRINSICS`):
/// the two leaf helpers' plain answers.
#[cfg(test)]
mod i137_l4_lf_leaf_intrinsic_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeSystemAccess};

    #[test]
    fn i137_l4_internal_member_name_by_slot_and_empty_spread() {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x7137_0004);
        let dmh_cid = ctx.ensure_class_initialized(LF_LEAF_DMH_CLASS).expect("dmh");
        ctx.set_declared_fields(
            dmh_cid,
            vec![cratonvm_native_api::FieldMetadata {
                name: "member".to_string(),
                descriptor: "Ljava/lang/invoke/MemberName;".to_string(),
                access_flags: 0x0010,
                slot_index: 2,
                declaring_class_id: dmh_cid,
                is_static: false,
            }],
        );
        let dmh = ctx.alloc_object(dmh_cid, 4);
        let member = ctx.alloc_object(dmh_cid, 4);
        ctx.set_field(dmh, 2, Value::Object(Some(member)));
        assert_eq!(dmh_member_slot(&ctx, dmh), Some(2));
        let r = dmh_internal_member_name_intrinsic(&mut ctx, &[Value::Object(Some(dmh))]);
        assert!(
            matches!(r, Ok(Some(Value::Object(Some(m)))) if m.as_ptr() == member.as_ptr()),
            "the member slot answers"
        );
        // A second call takes the admitted-class word.
        let r = dmh_internal_member_name_intrinsic(&mut ctx, &[Value::Object(Some(dmh))]);
        assert!(matches!(r, Ok(Some(Value::Object(Some(_))))));
        let r = mhi_check_spread_argument_intrinsic(&mut ctx, &[Value::Object(None), Value::Int(0)]);
        assert!(matches!(r, Ok(None)), "a null array of length 0 is accepted");
    }

    /// `CRATONVM_MHN_RESOLVE_SPECIAL_LINK_CHECKS`: `LinkResolver`'s three
    /// static-ness wordings (`L4W137FindSpecialAccess`).
    #[test]
    fn i137_l4_static_mismatch_wordings() {
        let mut ctx = MockNativeContext::new();
        let d = ctx.ensure_class_initialized("p/D").expect("p/D");
        assert_eq!(
            mhn_static_mismatch_message(&ctx, 6, false, d, "m", "()Ljava/lang/String;"),
            "Expected static method 'java.lang.String p.D.m()'"
        );
        assert_eq!(
            mhn_static_mismatch_message(&ctx, 7, false, d, "m", "(I)V"),
            "Expecting non-static method 'void p.D.m(int)'"
        );
        assert_eq!(
            mhn_static_mismatch_message(&ctx, 9, true, d, "m", "()I"),
            "Expected instance not static method 'int p.D.m()'"
        );
        assert_eq!(mhn_special_indirect_superinterface(&ctx, d, None, d, "m", "()V"), None);
        assert_eq!(mhn_special_indirect_superinterface(&ctx, d, Some(d), d, "m", "()V"), None);
    }
}

/// Interpreter round i7 wave 136, lane L3 (cross-review of wave 135 L4):
/// `CUSTOMIZE_THRESHOLD` is the JDK's own static once `MethodHandleStatics`
/// is initialized (a property written at run time moves nothing, as on
/// HotSpot), and the property parsed as `Integer.parseInt` before that.
#[cfg(test)]
mod i136_l3_customize_threshold_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeSystemAccess};

    const KEY: &str = "java.lang.invoke.MethodHandle.CUSTOMIZE_THRESHOLD";

    #[test]
    fn i136_l3_the_jdk_static_wins_over_a_later_property_write() {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x7136_0301);
        let statics = ctx.ensure_class_initialized("java/lang/invoke/MethodHandleStatics").expect("mhs");
        ctx.set_declared_fields(
            statics,
            vec![cratonvm_native_api::FieldMetadata {
                name: "CUSTOMIZE_THRESHOLD".to_string(),
                descriptor: "I".to_string(),
                access_flags: 0x0018,
                slot_index: 3,
                declaring_class_id: statics,
                is_static: true,
            }],
        );
        ctx.set_static_field(statics, 3, Value::Int(MH_CUSTOMIZE_THRESHOLD));
        ctx.set_system_property(KEY, "-1");
        assert_eq!(customize_threshold(&ctx), MH_CUSTOMIZE_THRESHOLD);
        assert!(customize_threshold_is_default(&ctx));
    }

    #[test]
    fn i136_l3_before_the_clinit_the_property_is_parsed_uncached() {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x7136_0302);
        assert_eq!(customize_threshold(&ctx), MH_CUSTOMIZE_THRESHOLD, "no property");
        ctx.set_system_property(KEY, "+127");
        assert!(customize_threshold_is_default(&ctx));
        ctx.set_system_property(KEY, "0");
        assert_eq!(customize_threshold(&ctx), 0, "not cached before the clinit");
        ctx.set_system_property(KEY, "-1");
        assert_eq!(customize_threshold(&ctx), -1);
        ctx.set_system_property(KEY, "-2");
        assert_eq!(customize_threshold(&ctx), MH_CUSTOMIZE_THRESHOLD_UNUSABLE);
        ctx.set_system_property(KEY, "x");
        assert!(!customize_threshold_is_default(&ctx));
    }
}

/// Interpreter round i9 (page i147-orch): `CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE`.
/// Every switch is set through a thread override; no default is pinned. The
/// end-to-end rows (cast rejections, `<init>` throwables, customization, a
/// stack walk from `<init>`, hand-built handles) are run against HotSpot.
/// Restored by t1 lane H (2026-10-25) with the lane the lang_invoke
/// decomposition had dropped; `the_lane_and_its_call_are_present` pins that.
#[cfg(test)]
mod i9_ctor_cast_adapter_lane_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeHeapAccess, NativeSystemAccess};
    use cratonvm_types::flags::with_thread_overrides;

    const SWITCH: &str = "CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE";
    const SITE: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";

    #[test]
    fn the_switch_reads_the_truthy_word_rule() {
        for (word, on) in [(Some("0"), false), (Some("1"), true), (Some("on"), true)] {
            with_thread_overrides(&[(SWITCH, word)], || {
                assert_eq!(crate::nbflags().mh_real_ctor_cast_adapter_lane, on, "{word:?}");
            });
        }
    }

    #[test]
    fn the_lane_declines_off_its_site_and_off_its_shape() {
        let mut ctx = MockNativeContext::new();
        let this = ctx.alloc_object(ClassId::new(0), 1);
        let arg = [Value::Object(None)];
        with_thread_overrides(&[(SWITCH, Some("1"))], || {
            // Another site, another arity, then a receiver that is no `Species_LL`.
            assert!(mh_real_ctor_cast_adapter_fast(&mut ctx, this, &arg, Some("()Ljava/lang/Object;")).is_none());
            assert!(mh_real_ctor_cast_adapter_fast(&mut ctx, this, &[], Some(SITE)).is_none());
            assert!(mh_real_ctor_cast_adapter_fast(&mut ctx, this, &arg, Some(SITE)).is_none());
            assert!(!mh_real_constructor_lane_may_serve(&ctx, this, SITE));
        });
        with_thread_overrides(&[(SWITCH, Some("0"))], || {
            assert!(!mh_real_constructor_lane_may_serve(&ctx, this, SITE));
        });
    }

    /// As gcj j2/x1 pins for the nullary lane: an armed `VMObjectAlloc`
    /// listener declines before the decode (whose memo hit would instantiate
    /// with no event), and the cast is checked before the count and `<init>`.
    #[test]
    fn the_armed_decline_precedes_the_decode_and_the_cast_precedes_init() {
        let src: &'static str = LANG_INVOKE_SRC;
        let at = src.find("fn mh_real_ctor_cast_adapter_fast(").expect("the lane");
        let end = src[at..].find("\n}\n").expect("fn end");
        let body = &src[at..at + end];
        let armed = body.find("ctx.jvmti_vm_object_alloc_armed()").expect("the decline");
        let decode = body.find("ctor_cast_adapter_decode(").expect("the decode");
        let cast = body.find("ctx.is_subclass(").expect("the cast");
        let count = body.find("lane_check_customized(").expect("the count");
        let init = body.find("new_object_initialized_with_class_id(").expect("the init");
        assert!(armed < decode && decode < cast && cast < count && count < init);
    }

    /// t1 lane H: a declared default-ON switch must have its lane AND the
    /// door's call of it (the decomposition dropped both silently, and the
    /// flag kept parsing).
    #[test]
    fn the_lane_and_its_call_are_present() {
        let src: &'static str = LANG_INVOKE_SRC;
        assert!(src.contains("fn mh_real_ctor_cast_adapter_fast("));
        assert!(src.contains("mh_real_ctor_cast_adapter_fast(ctx, this, &args[1..], call_site.as_deref())"));
        let at = src.find("pub fn mh_real_constructor_lane_may_serve(").expect("may_serve");
        let end = src[at..].find("\n}\n").expect("fn end");
        assert!(src[at..at + end].contains("ctor_cast_adapter_decode("));
    }

    /// t1 wave 8, lane I147: only the cast `asType` puts in front of the
    /// constructor (to its parameter's class) is the lane's;
    /// `filterReturnValue(ctor, castTo(Y))` over a `C(Object)` constructor has
    /// the same fields and casts the result instead.
    #[test]
    fn i147_the_cast_must_name_the_constructor_parameter() {
        let p = "Ljava/lang/CharSequence;";
        assert!(ctor_cast_names_param(Some("java/lang/CharSequence"), p));
        assert!(!ctor_cast_names_param(Some("java/lang/String"), p));
        assert!(!ctor_cast_names_param(None, p));
        // The return-filter shape: a parameter of `Object`, a cast to `Y`.
        assert!(!ctor_cast_names_param(Some("p/Y"), "Ljava/lang/Object;"));
        // A cast to `Object` never fails, so serving that one is the same.
        assert!(ctor_cast_names_param(Some("java/lang/Object"), "Ljava/lang/Object;"));
        assert!(!ctor_cast_names_param(Some("p/X"), "[Lp/X;"));
        assert!(!ctor_cast_names_param(Some("p/X"), "Lp/X"));
        let src: &'static str = LANG_INVOKE_SRC;
        let at = src.find("fn ctor_cast_adapter_shape(").expect("the shape");
        let end = src[at..].find("\n}\n").expect("fn end");
        assert!(src[at..at + end].contains("ctor_cast_names_param("));
    }
}

/// JIT round 21, lane vhmerge
/// (`docs/internal/fixed-bugs/jr21-vhmerge-varhandle-array-regression-from-dev-merge-FIXED-20261026.md`;
/// probes `R20FuzzGVhArrayAll1` refuse code 10, `R20FuzzIVhArr1` counts):
/// the static value check that runs BEFORE an array handle's coordinate cast
/// judges the values against the HANDLE's component type, so a wrong array
/// passed as `Object` is the cast's `ClassCastException`, while a value the
/// handle's own type refuses is still the earlier `WrongMethodTypeException`.
#[cfg(test)]
mod jr21_vhmerge_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::poly_call_site::{arm_vh_param_site, vh_param_site_classes};
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    fn obj(ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
        match ctx.new_object(class).unwrap() {
            Some(Value::Object(Some(o))) => o,
            _ => unreachable!(),
        }
    }

    fn handle(ctx: &mut MockNativeContext, class_name: &str, desc: &str) -> ObjectRef {
        let vh = obj(ctx, "java/lang/invoke/VarHandle");
        vh_meta_put(
            ctx,
            vh,
            VarHandleMeta {
                kind: VH_KIND_ARRAY,
                class_name: class_name.to_string(),
                field_name: String::new(),
                field_desc: desc.to_string(),
                field_index: -1,
                class_id: 0,
                read_only: false,
                exact: None,
                ref_type_class_id: 0,
                holder_class_id: 0,
            },
        );
        vh
    }

    fn is_cce(err: &MethodCallFailed) -> bool {
        matches!(
            err,
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { .. }
            ))
        )
    }

    fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
        match err {
            MethodCallFailed::ExceptionThrown(o) => {
                ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                    == Some("java/lang/invoke/WrongMethodTypeException")
            }
            _ => false,
        }
    }

    /// The judged variable type is the handle's, not the passed array's.
    #[test]
    fn the_static_value_type_is_the_handles_component() {
        if !vh_array_coordinate_cast_enabled() {
            return; // an explicit `0` in the test environment wins
        }
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x21AB_0001);
        let zh = handle(&mut ctx, "[Z", "Z");
        let ints = ctx.new_array(ArrayElementType::Int, 4);
        let args = [
            Value::Object(Some(zh)),
            Value::Object(Some(ints)),
            Value::Int(0),
            Value::Int(1),
        ];
        let meta = vh_meta_get(&mut ctx, zh);
        assert_eq!(
            vh_static_value_var_desc(&mut ctx, zh, meta.as_deref(), &args),
            Some(b'Z')
        );
        // The run-time reading (right only after the cast) says `int`.
        assert_eq!(
            vh_primitive_var_desc(&mut ctx, meta.as_deref(), &args),
            Some(b'I')
        );
        // A reference-array handle has no primitive variable.
        let oh = handle(&mut ctx, "[Ljava/lang/Object;", "Ljava/lang/Object;");
        let meta = vh_meta_get(&mut ctx, oh);
        let args = [
            Value::Object(Some(oh)),
            Value::Object(Some(ints)),
            Value::Int(0),
            Value::Int(1),
        ];
        assert_eq!(
            vh_static_value_var_desc(&mut ctx, oh, meta.as_deref(), &args),
            None
        );
    }

    /// `R20FuzzGVhArrayAll1` case 10 and `R20FuzzIVhArr1`'s `viaObject`:
    /// a wrong primitive array through an `Object` coordinate is CCE for a
    /// value mode, as on HotSpot 25; nothing is stored.
    #[test]
    fn a_wrong_primitive_array_in_a_value_mode_is_cce_not_wmte() {
        if !vh_array_coordinate_cast_enabled() {
            return;
        }
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x21AB_0002);
        let zv = Value::Object(Some(handle(&mut ctx, "[Z", "Z")));
        let lv = Value::Object(Some(handle(&mut ctx, "[J", "J")));
        let iv = Value::Object(Some(handle(&mut ctx, "[I", "I")));
        let ints = ctx.new_array(ArrayElementType::Int, 4);
        let bytes = ctx.new_array(ArrayElementType::Byte, 4);
        let a = Value::Object(Some(ints));
        let b = Value::Object(Some(bytes));
        // VZ.getAndBitwiseOr((Object) ia, 0, true)
        arm_vh_param_site(vh_param_site_classes("(Ljava/lang/Object;IZ)Z"));
        let err = varhandle_get_and_bitwise_or(&mut ctx, &[zv, a, Value::Int(0), Value::Int(1)])
            .unwrap_err();
        assert!(is_cce(&err), "boolean[] handle, int[] array: {err:?}");
        // LA.set((Object) ints, 1, 9L)
        arm_vh_param_site(vh_param_site_classes("(Ljava/lang/Object;IJ)V"));
        let err = varhandle_set(&mut ctx, &[lv, a, Value::Int(1), Value::Long(9)]).unwrap_err();
        assert!(is_cce(&err), "long[] handle, int[] array: {err:?}");
        assert_eq!(ctx.get_array_element(ints, 1), Value::Int(0));
        // IA.compareAndSet((Object) bytes, 1, 0, 1)
        arm_vh_param_site(vh_param_site_classes("(Ljava/lang/Object;III)Z"));
        let err = varhandle_compare_and_set(
            &mut ctx,
            &[iv, b, Value::Int(1), Value::Int(0), Value::Int(1)],
        )
        .unwrap_err();
        assert!(is_cce(&err), "int[] handle, byte[] array: {err:?}");
        // Positive control (dev's order fix stays): a value the HANDLE's own
        // type refuses is WMTE, on the right array too, and stores nothing.
        arm_vh_param_site(vh_param_site_classes("(Ljava/lang/Object;IJ)V"));
        let err = varhandle_set(&mut ctx, &[iv, a, Value::Int(1), Value::Long(9)]).unwrap_err();
        assert!(is_wmte(&ctx, &err), "int[] handle, long value: {err:?}");
        assert_eq!(ctx.get_array_element(ints, 1), Value::Int(0));
    }
}

/// j27 wave 9, lane W9EXC: an instance-field VarHandle access with a null
/// holder lists HotSpot's `VarHandle<T>s$FieldInstance*.<mode>` ->
/// `Objects.requireNonNull` frames (`J27NpeHiddenRows` `vh-get-null-holder`).
mod j27_vh_null_holder_chain_tests {
    use super::*;
    use cratonvm_native_api::ServedRaiseHopSite;

    #[test]
    fn instance_family_takes_the_holder_as_object() {
        assert_eq!(
            vh_field_mode_method("get", b'I', true),
            Some((
                "java/lang/invoke/VarHandleInts$FieldInstanceReadOnly",
                "(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;)I"
            ))
        );
        assert_eq!(
            vh_field_mode_method("set", b'L', true),
            Some((
                "java/lang/invoke/VarHandleReferences$FieldInstanceReadWrite",
                "(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;Ljava/lang/Object;)V"
            ))
        );
        assert_eq!(
            vh_field_mode_method("getAndAdd", b'J', true),
            Some((
                "java/lang/invoke/VarHandleLongs$FieldInstanceReadWrite",
                "(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;J)J"
            ))
        );
        // The static family is unchanged by the generalisation.
        assert_eq!(vh_field_mode_method("get", b'I', false), vh_static_mode_method("get", b'I'));
    }

    #[test]
    fn null_holder_chain_tops_out_at_require_non_null() {
        let chain = vh_null_holder_chain("set", b'I').unwrap();
        assert_eq!(chain.throwable_class, "java/lang/NullPointerException");
        let hops: Vec<(&str, &str)> = chain.hops.iter().map(|h| (h.class, h.name)).collect();
        assert_eq!(
            hops,
            [
                ("java/lang/invoke/VarHandle", "set"),
                ("java/lang/invoke/VarHandleInts$FieldInstanceReadWrite", "set"),
                ("java/util/Objects", "requireNonNull"),
            ]
        );
        assert_eq!(chain.hops[0].site, ServedRaiseHopSite::Hidden);
        assert_eq!(chain.hops[2].site, ServedRaiseHopSite::Raise);
        assert!(vh_null_holder_chain("toMethodHandle", b'I').is_none());
        assert!(vh_null_holder_chain("get", 0).is_none());
    }
}
