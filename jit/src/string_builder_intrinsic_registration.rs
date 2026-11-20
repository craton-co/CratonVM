    use super::*;

    fn builder_layout() -> StringFieldLayout {
        StringFieldLayout::new(0, Some(1), 2, 7)
            .with_builder(StringBuilderFieldLayout::new(0, 1, 2, 9))
    }

    /// EVERY entry the resolver can hand back for a `java/lang/StringBuilder`
    /// site must be one `string_intrinsic_declines_to_a_call` claims.
    ///
    /// The two are asked at different places — the resolver at registration,
    /// the predicate by both doors to decide whether to mint a
    /// `JitInvokeInfo` — and a member added to one and not the other
    /// registers an intrinsic whose decline edge has no dispatch to decline
    /// into. The emitter then refuses the site, and that refusal fails the
    /// whole method, which is a silent drop to the interpreter rather than a
    /// visible error.
    #[test]
    fn every_string_builder_entry_is_one_that_declines_to_a_call() {
        let layout = Some(builder_layout());
        let mut seen = 0;
        for (name, descriptor) in [
            ("length", "()I"),
            ("append", "(C)Ljava/lang/StringBuilder;"),
            ("charAt", "(I)C"),
            ("append", "(Ljava/lang/String;)Ljava/lang/StringBuilder;"),
            ("append", "(I)Ljava/lang/StringBuilder;"),
        ] {
            let hit =
                try_resolve_string_intrinsic("java/lang/StringBuilder", name, descriptor, layout);
            let (entry, _, _, guard) = hit.unwrap_or_else(|| {
                panic!("the resolver must answer for StringBuilder.{name}{descriptor}")
            });
            assert!(
                string_intrinsic_declines_to_a_call(entry),
                "StringBuilder.{name}{descriptor} registers an entry the doors will not \
                 mint a JitInvokeInfo for"
            );
            assert_ne!(
                guard, 0,
                "a StringBuilder site must carry its class-id guard: without it the inline \
                 decode would run against a StringBuffer receiver, whose append is \
                 synchronized and whose toStringCache must be invalidated"
            );
            seen += 1;
        }
        assert_eq!(seen, 5);
    }

    /// A String / CharSequence member must NOT claim to decline into a call:
    /// every one of its edges is an uncommon trap, and minting a
    /// `JitInvokeInfo` for it would be dead weight on every String site in the
    /// program.
    #[test]
    fn the_string_members_still_decline_into_a_trap() {
        let layout = Some(builder_layout());
        for (class, name, descriptor) in [
            ("java/lang/String", "length", "()I"),
            ("java/lang/String", "charAt", "(I)C"),
            ("java/lang/CharSequence", "length", "()I"),
        ] {
            if let Some((entry, _, _, _)) =
                try_resolve_string_intrinsic(class, name, descriptor, layout)
            {
                assert!(
                    !string_intrinsic_declines_to_a_call(entry),
                    "{class}.{name}{descriptor} is trap-declining and must not be listed"
                );
            }
        }
    }

    /// A layout with no builder half leaves every StringBuilder site alone.
    #[test]
    fn without_a_builder_layout_no_string_builder_site_is_claimed() {
        let layout = Some(StringFieldLayout::new(0, Some(1), 2, 7));
        assert!(try_resolve_string_intrinsic(
            "java/lang/StringBuilder",
            "append",
            "(C)Ljava/lang/StringBuilder;",
            layout
        )
        .is_none());
    }

    /// `append(String)` needs the ARGUMENT's coder (the outer layout, not
    /// the builder's own), so a `has_coder == false` layout — a synthetic
    /// image whose `String` is the legacy `char[]` shape — leaves it on
    /// ordinary dispatch, same as every other coder-dependent String member.
    /// `append(char)`, `length()` and `charAt(I)C` do not read the
    /// argument's coder at all and stay claimed.
    #[test]
    fn append_string_without_the_arguments_coder_is_not_claimed() {
        let layout = Some(
            StringFieldLayout::new(0, None, 2, 7)
                .with_builder(StringBuilderFieldLayout::new(0, 1, 2, 9)),
        );
        assert!(try_resolve_string_intrinsic(
            "java/lang/StringBuilder",
            "append",
            "(Ljava/lang/String;)Ljava/lang/StringBuilder;",
            layout
        )
        .is_none());
        for (name, descriptor) in [
            ("length", "()I"),
            ("append", "(C)Ljava/lang/StringBuilder;"),
            ("charAt", "(I)C"),
            ("append", "(I)Ljava/lang/StringBuilder;"),
        ] {
            assert!(
                try_resolve_string_intrinsic("java/lang/StringBuilder", name, descriptor, layout)
                    .is_some(),
                "StringBuilder.{name}{descriptor} does not need the argument's coder and \
                 should stay claimed"
            );
        }
    }

    /// `StringBuffer` is never claimed, whatever the layout says. Its `append`
    /// is `synchronized` and it carries a `toStringCache` that every mutation
    /// must invalidate; the inline path emits neither.
    #[test]
    fn string_buffer_is_never_claimed() {
        let layout = Some(builder_layout());
        for (name, descriptor) in [
            ("length", "()I"),
            ("append", "(C)Ljava/lang/StringBuffer;"),
            ("append", "(C)Ljava/lang/StringBuilder;"),
        ] {
            assert!(
                try_resolve_string_intrinsic("java/lang/StringBuffer", name, descriptor, layout)
                    .is_none(),
                "StringBuffer.{name}{descriptor} must stay on native dispatch"
            );
        }
    }

    /// A zero class id is refused by the layout constructor: an unguarded
    /// decode of an arbitrary receiver is the hazard the guard exists for.
    #[test]
    fn a_layout_without_a_class_id_is_refused() {
        assert!(StringBuilderFieldLayout::new(0, 1, 2, 0).is_none());
    }

    /// JIT round 17 wave 6, lane sbzgc2 (SBZGC17-2): a fresh layout carries no
    /// `maybeLatin1`, and attaching one is refused -- `None`, and `false`
    /// never reported exact -- without an index or without a registered
    /// one-byte compact slot (class id 9 has no registered layout here).
    #[test]
    fn r17_sbzgc2_maybe_latin1_needs_a_registered_one_byte_slot() {
        let Some(b) = StringBuilderFieldLayout::new(0, 1, 2, 9) else {
            return; // CRATONVM_NO_JIT_SB_INTRINSICS armed in this environment
        };
        assert_eq!(b.maybe_latin1_compact_offset, None);
        assert!(!b.maybe_latin1_false_is_exact);
        let no_index = b.with_maybe_latin1(None, true);
        assert_eq!(no_index.maybe_latin1_compact_offset, None);
        assert!(!no_index.maybe_latin1_false_is_exact);
        if cratonvm_types::compact_field_disp_storage(9, 3).is_none() {
            let unregistered = b.with_maybe_latin1(Some(3), true);
            assert_eq!(unregistered.maybe_latin1_compact_offset, None);
            assert!(!unregistered.maybe_latin1_false_is_exact);
        }
        // Re-attaching replaces, never accumulates.
        assert_eq!(
            b.with_maybe_latin1(Some(3), true)
                .with_maybe_latin1(None, false),
            no_index
        );
    }

    /// JIT round 17 wave 1, lane strings (STR16-P1): `toString()` is claimed
    /// exactly when `CRATONVM_JIT_SB_TOSTRING_INLINE` is armed, then as a
    /// guarded member that declines into a call (so both doors mint its
    /// `JitInvokeInfo`), and never without the String layout's `coder` (the
    /// inline body builds a `String`) nor for `StringBuffer`.
    #[test]
    fn r17_strings_to_string_is_claimed_only_under_its_switch() {
        let layout = Some(builder_layout());
        let hit = try_resolve_string_intrinsic(
            "java/lang/StringBuilder",
            "toString",
            "()Ljava/lang/String;",
            layout,
        );
        if jit_sb_to_string_inline_enabled() {
            let (entry, num_params, ret, guard) = hit.expect("armed: claimed");
            assert_eq!(entry, JitIntrinsic::StringBuilderToString.as_entry());
            assert_eq!((num_params, ret), (0, b'L'));
            assert_ne!(guard, 0);
            assert!(string_intrinsic_declines_to_a_call(entry));
        } else {
            assert!(hit.is_none(), "unarmed: the call stays the JDK body");
        }
        let no_coder = Some(
            StringFieldLayout::new(0, None, 2, 7)
                .with_builder(StringBuilderFieldLayout::new(0, 1, 2, 9)),
        );
        assert!(try_resolve_string_intrinsic(
            "java/lang/StringBuilder",
            "toString",
            "()Ljava/lang/String;",
            no_coder
        )
        .is_none());
        assert!(try_resolve_string_intrinsic(
            "java/lang/StringBuffer",
            "toString",
            "()Ljava/lang/String;",
            layout
        )
        .is_none());
        assert!(string_intrinsic_declines_to_a_call(
            JitIntrinsic::StringBuilderToString.as_entry()
        ));
    }

    // JIT r23 wave 1, lane L5 (INTRINSICS22-1): both code-point rows are
    // claimed exactly under their own switch, each as a member that declines
    // into a call (so both doors mint its `JitInvokeInfo`); the builder row
    // carries its class-id guard and is never `StringBuffer`'s, the String
    // row is unguarded and never a `CharSequence` site's.
// JIT r23 wave 2, lane L5 (R23W1L5-2 / R23W1L5-3): `String.codePointBefore`
    // / `codePointCount` and `StringBuilder.codePointAt` are claimed exactly
    // under their switches, each declining to a call; the builder row carries
    // its guard, the String rows none.

