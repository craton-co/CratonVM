    use super::*;

    /// Pins the frame-push locals layout against the resize-then-overwrite form
    /// it replaced.
    ///
    /// `init_locals_from_parts` used to size both buffers to `max_locals` and
    /// then copy the arguments over the leading slots; it now pushes the
    /// arguments and resizes the remainder, so each slot is written once. The
    /// two must produce byte-identical buffers, or a frame starts life with the
    /// wrong local — which is a silent wrong value, and for a reference slot a
    /// GC-root question.
    ///
    /// Covers the three shapes that make the layouts differ: category-2
    /// arguments (two slots, upper one filler), a tail of unset locals, and the
    /// defensive clamp where the arguments alone would overrun `max_locals`.
    #[test]
    fn pushed_locals_match_the_resize_then_copy_layout() {
        fn reference(max_locals: u16, args: &[Value]) -> (Vec<CompactValue>, Vec<u8>) {
            let n = effective_max_locals(max_locals, args) as usize;
            let mut locals = vec![CompactValue::uninitialized(); n];
            let mut kinds = vec![LKIND_OTHER; n];
            copy_args_to_locals(&mut locals, &mut kinds, args);
            (locals, kinds)
        }

        let obj_free_cases: Vec<(u16, Vec<Value>)> = vec![
            (0, vec![]),
            (4, vec![]),
            (4, vec![Value::Int(7)]),
            (4, vec![Value::Int(1), Value::Int(2)]),
            // Category-2: each takes two slots with the upper half unset.
            (4, vec![Value::Long(0x1234_5678_9abc_def0)]),
            (4, vec![Value::Double(-0.0)]),
            (6, vec![Value::Long(1), Value::Int(2), Value::Double(3.5)]),
            // Mixed with a null reference, and with a retaddr.
            (5, vec![Value::Object(None), Value::Int(9)]),
            (5, vec![Value::ReturnAddress(11), Value::Float(1.5)]),
            // Declared max_locals SMALLER than the arguments need: the
            // effective count is clamped up, and both forms must agree on it.
            (1, vec![Value::Long(5), Value::Long(6)]),
            (0, vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        ];

        for (max_locals, args) in obj_free_cases {
            let (want_locals, want_kinds) = reference(max_locals, &args);
            let (got_locals, got_kinds, eff) = init_locals_from_parts(max_locals, &args, None);
            assert_eq!(
                eff as usize,
                want_locals.len(),
                "effective_max_locals for max_locals={max_locals} args={args:?}"
            );
            assert_eq!(
                got_locals.iter().map(|c| c.raw_bits()).collect::<Vec<_>>(),
                want_locals.iter().map(|c| c.raw_bits()).collect::<Vec<_>>(),
                "locals differ for max_locals={max_locals} args={args:?}"
            );
            assert_eq!(
                got_kinds, want_kinds,
                "local kinds differ for max_locals={max_locals} args={args:?}"
            );
        }
    }

    /// The same equivalence when the buffers come from the pool with stale
    /// content in them — the recycled bytes must not survive into either form.
    #[test]
    fn pushed_locals_do_not_leak_recycled_slots() {
        let args = vec![Value::Int(3)];
        let dirty_vals: Vec<u64> = vec![0xDEAD_BEEF_DEAD_BEEF; 16];
        let dirty_kinds: Vec<u8> = vec![LKIND_LONG; 16];
        let (locals, kinds, eff) =
            init_locals_from_parts(4, &args, Some((dirty_vals, dirty_kinds)));
        assert_eq!(eff, 4);
        assert_eq!(locals.len(), 4);
        assert_eq!(kinds.len(), 4);
        assert_eq!(locals[0].as_int(), Some(3));
        assert_eq!(kinds[0], LKIND_OTHER);
        for i in 1..4 {
            assert_eq!(
                locals[i].raw_bits(),
                CompactValue::uninitialized().raw_bits(),
                "slot {i} kept recycled content"
            );
            assert_eq!(kinds[i], LKIND_OTHER, "kind {i} kept recycled content");
        }
    }

    #[test]
    fn frame_creation_with_args() {
        let frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            5,
            &[Value::Int(42), Value::Long(100)],
        );

        assert_eq!(frame.locals_len(), 5);
        assert_eq!(frame.get_local(0).as_int(), Some(42));
        // CompactValue stores Long untagged, so `get_local(...).as_long()`
        // on the `Value` round-trip cannot distinguish Long from Double.
        // Use the CompactValue accessor with context (Long-by-instruction).
        assert_eq!(frame.get_local_compact(1).as_long(), Some(100));
        // Slot 2 is the second half of the long → Uninitialized
    }

    // ── CR-CLO-2 — `Frame::method_index` ────────────────────────────────
    //
    // The index makes deferred stack-trace line resolution EXACT across an
    // overload set. Its whole failure mode is staleness: `resolve_line_numbers_
    // in_place` guards an index by re-checking the method's NAME, and a name
    // check cannot separate two overloads — which is the one population the
    // index exists for. So the two places that reuse a `Frame` allocation
    // under a new method get dedicated tests.

    fn probe_frame(class_id: ClassId, method: &str, descriptor: &str) -> Frame {
        Frame::new(
            class_id,
            "probe/Target".to_string(),
            method.to_string(),
            descriptor.to_string(),
            Some("Target.java".to_string()),
            vec![0xb1],
            vec![],
            1,
            1,
            &[],
        )
    }

    #[test]
    fn a_frames_method_index_starts_absent_and_round_trips_through_the_setter() {
        // `None` is the correct default everywhere: it restores exactly
        // today's unambiguous-name behaviour, so a pusher that does not know
        // the slot never has to guess.
        let mut f = probe_frame(ClassId::new(0), "m", "(I)V");
        assert_eq!(f.method_index(), None);
        f.set_method_index(Some(7));
        assert_eq!(f.method_index(), Some(7));
        f.set_method_index(None);
        assert_eq!(f.method_index(), None);
    }

    #[test]
    fn a_thawed_continuation_frame_carries_no_method_index() {
        // `FrozenFrame` round-trips names and a descriptor, never a slot, and a
        // continuation can be resumed long after a redefinition reordered the
        // overload set it was captured from. Thaw must therefore start from
        // "unknown", not from whatever the pre-freeze frame happened to hold.
        let mut f = probe_frame(ClassId::new(0), "m", "(I)V");
        f.set_method_index(Some(5));
        let frozen = f.to_frozen_frame();
        let thawed = Frame::from_frozen_frame(frozen);
        assert_eq!(thawed.method_index(), None);
        assert_eq!(thawed.method_name(), "m");
    }

    /// Interpreter round i6 wave 118, lane L6: a freeze and thaw keep the
    /// frame's monitors -- a synchronized method's `monitor_on_exit`, the
    /// block-monitor record in order, and its understated mark. Before, the
    /// thawed frame held none of them.
    #[test]
    fn a_thawed_frame_keeps_its_monitors() {
        // SAFETY: never dereferenced; the frame only carries the addresses.
        let obj = |a: usize| unsafe { crate::types::ObjectRef::from_raw(a as *mut u8) };
        let mut f = probe_frame(ClassId::new(0), "m", "(I)V");
        f.monitor_on_exit = Some(obj(0x1000));
        f.held_monitors.push(obj(0x2000));
        f.held_monitors.push(obj(0x3000));
        let thawed = Frame::from_frozen_frame(f.to_frozen_frame());
        assert_eq!(thawed.monitor_on_exit, Some(obj(0x1000)));
        assert_eq!(thawed.held_monitors.as_slice(), &[obj(0x2000), obj(0x3000)]);
        assert!(!thawed.held_monitors.is_understated());
        let mut g = probe_frame(ClassId::new(0), "m", "(I)V");
        g.held_monitors.mark_understated();
        let thawed = Frame::from_frozen_frame(g.to_frozen_frame());
        assert!(thawed.held_monitors.is_understated());
        assert!(thawed.held_monitors.is_empty());
    }

    /// A `CachedBytecodeMethod` shaped for the frame-install tests.
    fn cached_probe(
        name: &str,
        descriptor: &str,
        max_locals: u16,
        max_stack: u16,
    ) -> Arc<CachedBytecodeMethod> {
        cached_probe_with_code(name, descriptor, max_locals, max_stack, &[0xb1])
    }

    /// [`cached_probe`] running `code` (a STATIC method, as `cached_probe`).
    /// gci i3/il: a static method's slot 0 is a parameter, not a receiver, so
    /// a test whose argument must stay a root gives the body a read of it.
    fn cached_probe_with_code(
        name: &str,
        descriptor: &str,
        max_locals: u16,
        max_stack: u16,
        code: &[u8],
    ) -> Arc<CachedBytecodeMethod> {
        Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(0),
                class_name: Arc::from("probe/Target"),
                method_name: Arc::from(name),
                method_descriptor: Arc::from(descriptor),
                source_file: Some(Arc::from("Target.java")),
                code: padded_bytecode(code),
                exception_table: Arc::from(Vec::<ExceptionTableEntry>::new().into_boxed_slice()),
                max_stack,
                max_locals,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        ))
    }

    /// gci i4/il: an `Owned` frame (the loose-parts constructors) keeps slot 0
    /// live until a door records that its method is static; then slot 0 is
    /// filtered by liveness (dead at a bare `return`), and a redefinition's
    /// move onto a new body keeps the answer. A thawed frame does not know it.
    #[test]
    fn gci_i4il_an_owned_static_frame_filters_its_dead_slot0_once_told() {
        let mut f = probe_frame(ClassId::new(0), "relay", "([J)V");
        assert!(f.owned_meta().is_some(), "Frame::new builds an Owned frame");
        assert!(f.liveness_keeps_slot0());
        assert!(f.live_locals_mask_here().is_live(0), "unknown static-ness keeps slot 0");

        f.note_method_is_static(true);
        assert!(!f.liveness_keeps_slot0());
        assert!(
            !f.live_locals_mask_here().is_live(0),
            "a static method's slot 0 is dead at its `return`"
        );

        let _ = f.adopt_redefined_body(
            padded_bytecode(&[0xb1]),
            Arc::from(Vec::<ExceptionTableEntry>::new().into_boxed_slice()),
            0,
            false,
            None,
        );
        assert!(!f.liveness_keeps_slot0(), "a redefinition keeps the method static");

        f.note_method_is_static(false);
        assert!(f.liveness_keeps_slot0());
        assert!(f.live_locals_mask_here().is_live(0), "an instance method keeps its receiver");

        let mut s = probe_frame(ClassId::new(0), "relay", "([J)V");
        s.note_method_is_static(true);
        let thawed = Frame::from_frozen_frame(s.to_frozen_frame());
        assert!(thawed.liveness_keeps_slot0(), "a thawed frame does not know: keeps slot 0");
    }

    /// gci i4/il: on a cached frame the method record decides; the door's
    /// note is a no-op there.
    #[test]
    fn gci_i4il_a_cached_frame_ignores_the_static_note() {
        let mut f = Frame::new_cached_unpooled(cached_probe("relay", "([J)V", 1, 1), &[]);
        assert!(f.owned_meta().is_none(), "new_cached_unpooled builds a Cached frame");
        assert!(!f.liveness_keeps_slot0(), "cached_probe is static");
        f.note_method_is_static(false);
        assert!(!f.liveness_keeps_slot0(), "the CachedBytecodeMethod still decides");
        assert!(!f.live_locals_mask_here().is_live(0));
    }

    /// Everything a caller can observe about an installed frame, so the three
    /// install paths can be compared as wholes rather than field by field
    /// (`seq` excluded: it is a fresh counter value by design).
    fn frame_shape(f: &Frame) -> (ClassId, usize, u16, u16, usize, Vec<u64>, Vec<u8>) {
        (
            f.class_id,
            f.pc,
            f.max_stack,
            f.max_locals,
            f.stack.len(),
            f.locals.iter().map(|c| c.raw_bits()).collect(),
            f.local_kinds.to_vec(),
        )
    }

    #[test]
    fn rebuilding_a_retired_slot_holds_what_a_by_value_push_held() {
        // The claim the general dispatchers' slot reuse rests on. If these two
        // can differ, a call that happens to find a retired slot runs with
        // different locals from the same call that does not — which is a
        // wrong-answer bug that no test of either path alone can see.
        let cached = cached_probe("m", "(IJLjava/lang/Object;)V", 6, 3);
        let args = [
            Value::Int(7),
            Value::Long(-9_000_000_000),
            Value::Object(None),
        ];

        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();
        let fresh = Frame::new_pooled_cached(
            Arc::clone(&cached),
            &args,
            &mut locals_pool,
            &mut stacks_pool,
        );

        // A frame that has been used and is now being rebuilt: dirty locals, a
        // non-empty operand stack, a pc partway through, a monitor to release.
        let mut used = Frame::new_pooled_cached(
            cached_probe("other", "(D)V", 12, 9),
            &[Value::Double(1.5)],
            &mut locals_pool,
            &mut stacks_pool,
        );
        used.pc = 17;
        used.backward_count = 42;
        let _ = used.stack.push(Value::Int(1234));
        used.reset_cached_value(Arc::clone(&cached), &args);

        assert_eq!(frame_shape(&used), frame_shape(&fresh));
        assert_eq!(used.backward_count, 0);
        assert!(used.monitor_on_exit.is_none());
        assert_eq!(used.method_name(), "m");
    }

    #[test]
    fn the_value_and_compact_resets_lay_down_the_same_locals() {
        // `reset_cached_value` and `reset_cached_compact` are the two argument
        // representations of one operation. A `long` is the case that can
        // diverge: it occupies two local slots and the upper half is filler.
        let cached = cached_probe("m", "(JI)V", 4, 2);
        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();

        let mut by_value =
            Frame::new_pooled_cached(Arc::clone(&cached), &[], &mut locals_pool, &mut stacks_pool);
        by_value.reset_cached_value(Arc::clone(&cached), &[Value::Long(5), Value::Int(6)]);

        let mut by_slot =
            Frame::new_pooled_cached(Arc::clone(&cached), &[], &mut locals_pool, &mut stacks_pool);
        by_slot.reset_cached_compact(
            Arc::clone(&cached),
            &[
                (CompactValue::from_value_kinded(Value::Long(5)), b'J'),
                (CompactValue::from_value_kinded(Value::Int(6)), b'I'),
            ],
        );

        assert_eq!(frame_shape(&by_slot), frame_shape(&by_value));
    }

    #[test]
    fn an_emplaced_value_frame_matches_a_pushed_one() {
        // The other half of the ladder: no retired slot, so the frame is
        // written into the next slot instead of being built and moved.
        let cached = cached_probe("m", "(FI)V", 5, 4);
        let args = [Value::Float(2.5), Value::Int(-3)];
        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();

        let mut pushed = FrameStack::new();
        pushed.push(Frame::new_pooled_cached(
            Arc::clone(&cached),
            &args,
            &mut locals_pool,
            &mut stacks_pool,
        ));

        let mut emplaced = FrameStack::new();
        let (locals, kinds, stack, eff) =
            take_cached_value_parts(&cached, &args, &mut locals_pool, &mut stacks_pool);
        emplaced.emplace_cached_compact(Arc::clone(&cached), locals, kinds, stack, eff);

        assert_eq!(emplaced.len(), 1);
        assert_eq!(frame_shape(&emplaced[0]), frame_shape(&pushed[0]));
    }

    #[test]
    fn a_value_reuse_push_is_refused_when_no_slot_is_retired() {
        // `push_cached_value_reusing` must never grow the stack: the caller
        // reads `false` as "build one", and a version that pushed anyway would
        // install the frame twice.
        let cached = cached_probe("m", "()V", 1, 1);
        let mut frames = FrameStack::new();
        assert!(!frames.push_cached_value_reusing(Arc::clone(&cached), &[]));
        assert_eq!(frames.len(), 0);

        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();
        frames.push(Frame::new_pooled_cached(
            Arc::clone(&cached),
            &[],
            &mut locals_pool,
            &mut stacks_pool,
        ));
        assert!(frames.retire_top());
        assert!(frames.push_cached_value_reusing(cached, &[]));
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn a_cached_frame_reports_no_index_until_the_shared_entry_carries_one() {
        // Pins the documented state of the cached half: `CachedBytecodeMethod`
        // has no `method_index` field (its 38 struct literals live in four
        // crates and none has a `..` tail), so a frame pushed through the
        // cached-invoke path answers `None` and falls back to the name rule.
        // When that field lands, `new_pooled_cached` seeds this from the Arc
        // and this assertion flips to `Some(..)`.
        let cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(0),
                class_name: Arc::from("probe/Target"),
                method_name: Arc::from("m"),
                method_descriptor: Arc::from("(I)V"),
                source_file: Some(Arc::from("Target.java")),
                code: padded_bytecode(&[0xb1]),
                exception_table: Arc::from(Vec::<ExceptionTableEntry>::new().into_boxed_slice()),
                max_stack: 1,
                max_locals: 1,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        ));
        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();
        let mut f = Frame::new_pooled_cached(cached, &[], &mut locals_pool, &mut stacks_pool);
        assert_eq!(f.method_index(), None);
        // The per-frame field is still writable for a cached frame — the
        // uniform placement on `Frame` is what buys that.
        f.set_method_index(Some(1));
        assert_eq!(f.method_index(), Some(1));
    }

    /// The payoff, end to end: an overload set resolves to an EXACT line when
    /// the frame carries its index, and fails closed to `UNKNOWN` without it.
    ///
    /// This is written against the entry shape
    /// `stackwalker::capture_frames_no_lines` produces once its `method_index:
    /// None` becomes `method_index: f.method_index()` (one line, in a file this
    /// pass does not own). Everything the change depends on is pinned here.
    #[test]
    fn an_overload_set_resolves_exactly_only_when_the_frame_carries_its_index() {
        use crate::runtime::stackwalker::{resolve_line_numbers_in_place, LINE_NUMBER_UNKNOWN};
        use cratonvm_reader::attribute::LineNumberEntry;

        // Two overloads of `m`: same name, different LineNumberTables. The
        // unambiguous-name rule cannot choose between them by construction.
        let (store, class_id) = crate::runtime::stackwalker::test_support::store_with(vec![
            crate::runtime::stackwalker::test_support::named_method(
                "m",
                "(I)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 11,
                }],
            ),
            crate::runtime::stackwalker::test_support::named_method(
                "m",
                "(J)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 22,
                }],
            ),
        ]);

        let entry_for = |f: &Frame| cratonvm_native_api::StackTraceEntry {
            class_name: f.class_name_arc(),
            method_name: f.method_name_arc(),
            method_descriptor: None,
            source_file: f.source_file_arc(),
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: f.last_instr_pc.min(i32::MAX as usize) as i32,
            class_id: Some(f.class_id),
            method_index: f.method_index(),
        };

        // Without an index: declines, and specifically does NOT guess.
        let plain = probe_frame(class_id, "m", "(J)V");
        let mut entries = vec![entry_for(&plain)];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
        assert_eq!(entries[0].line_number, LINE_NUMBER_UNKNOWN);

        // With the index: exact, and it picks the SECOND overload — the one a
        // name-only rule could never have reached.
        let mut indexed = probe_frame(class_id, "m", "(J)V");
        indexed.set_method_index(Some(1));
        let mut entries = vec![entry_for(&indexed)];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 1);
        assert_eq!(entries[0].line_number, 22);

        // And the first overload resolves to its own line, not the other's.
        let mut first = probe_frame(class_id, "m", "(I)V");
        first.set_method_index(Some(0));
        let mut entries = vec![entry_for(&first)];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 1);
        assert_eq!(entries[0].line_number, 11);
    }

    /// If `Code.max_locals` is smaller than the invocation argument slots,
    /// the frame must still receive every argument (Surefire-style bad metadata).
    #[test]
    fn frame_expands_locals_when_declared_max_smaller_than_invoke_args() {
        let args = [Value::Int(10), Value::Int(20), Value::Int(30)];
        let frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            4,
            2,
            &args,
        );
        assert_eq!(frame.max_locals, 3);
        assert_eq!(frame.locals_len(), 3);
        assert_eq!(frame.get_local(0).as_int(), Some(10));
        assert_eq!(frame.get_local(1).as_int(), Some(20));
        assert_eq!(frame.get_local(2).as_int(), Some(30));
    }

    #[test]
    fn frame_expands_locals_for_category2_when_declared_too_small() {
        let args = [Value::Long(0x1122_3344_5566_7788)];
        let frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            4,
            1,
            &args,
        );
        assert_eq!(frame.max_locals, 2);
        assert_eq!(frame.locals_len(), 2);
        // Long/Double tag ambiguity in CompactValue — use the compact
        // accessor with explicit Long context.
        assert_eq!(
            frame.get_local_compact(0).as_long(),
            Some(0x1122_3344_5566_7788)
        );
        assert_eq!(frame.get_local(1), Value::Uninitialized);
    }

    #[test]
    fn frame_set_and_get_local() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            3,
            &[],
        );

        frame.set_local(0, Value::Int(1));
        frame.set_local(1, Value::Float(2.5));
        assert_eq!(frame.get_local(0).as_int(), Some(1));
        assert_eq!(frame.get_local(1).as_float(), Some(2.5));
    }

    #[test]
    fn frame_soa_all_types() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            8,
            &[],
        );

        frame.set_local(0, Value::Int(42));
        frame.set_local(1, Value::Long(9999999999));
        frame.set_local(2, Value::Float(3.15));
        frame.set_local(3, Value::Double(2.719));
        frame.set_local(4, Value::Object(None));
        frame.set_local(5, Value::Uninitialized);
        frame.set_local(6, Value::ReturnAddress(123));

        assert_eq!(frame.get_local(0).as_int(), Some(42));
        // Long via the context-aware compact accessor.
        assert_eq!(frame.get_local_compact(1).as_long(), Some(9999999999));
        // Bit equality, like the int/long/object slots asserted beside them:
        // a local read back is a round trip with no arithmetic on the path.
        assert_eq!(
            frame.get_local(2).as_float().unwrap().to_bits(),
            3.15f32.to_bits()
        );
        assert_eq!(
            frame.get_local(3).as_double().unwrap().to_bits(),
            2.719f64.to_bits()
        );
        assert!(frame.get_local(4).is_null());
        assert_eq!(frame.get_local(5), Value::Uninitialized);
        if let Value::ReturnAddress(a) = frame.get_local(6) {
            assert_eq!(a, 123);
        } else {
            panic!("expected ReturnAddress");
        }
    }

    #[test]
    fn frame_get_local_raw() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            3,
            &[Value::Int(42), Value::Long(100)],
        );

        assert_eq!(frame.get_local_raw(0), 42);
        assert_eq!(frame.get_local_raw(1) as i64, 100);
    }

    /// REGRESSION (OSR entry-state reconstruction / `CRATONVM_JIT_OSR`): a
    /// primitive `long` whose NaN-boxed bits collide with the `SUB_OBJECT`
    /// sub-tag must still round-trip through `get_local_raw`/`get_local_tag`
    /// bit-exact as `VTAG_LONG`. `try_osr` is this pair's only production
    /// caller (building the `jit_locals` snapshot handed to `osr_enter`); before
    /// the `local_kinds` gate, `compact_to_local_slot` classified such a slot
    /// as `VTAG_OBJECT` purely from `CompactValue::tag()` and returned only its
    /// low 47-bit payload — silently truncating the long's true value into a
    /// fabricated "pointer" at OSR entry, mirroring the exact hazard
    /// `scan_local_objects_skips_collision_long_aliasing_live_object` and
    /// `locals_snapshot`'s `local_kinds` gate already guard against elsewhere.
    #[test]
    fn get_local_raw_preserves_collision_long_as_long_not_truncated_object() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        // Bit-identical to a genuine `CompactValue::object(addr)` for some
        // plausible-looking, 8-byte-aligned, above-guard-page address — but
        // stored as a `long`, so `local_kinds[0]` is `LKIND_LONG`.
        let addr = 0x20000000000u64;
        assert_eq!(addr & 0x7, 0);
        assert!(addr < (1 << 47));
        let collision_bits = CompactValue::object(addr).raw_bits();
        frame.set_local_unchecked(0, Value::Long(collision_bits as i64));

        // Sanity: the underlying CompactValue really is tag-ambiguous.
        assert!(frame.get_local_compact(0).is_object());

        assert_eq!(
            frame.get_local_raw(0),
            collision_bits,
            "get_local_raw must return the bit-exact long, not the masked 47-bit payload"
        );
        assert_eq!(frame.get_local_tag(0), VTAG_LONG);
    }

    /// A `VTAG_LONG` local whose bits happen to look like an aligned object
    /// pointer is **NOT** a heap root — primitives are not references by JVM
    /// spec, and the bridges that used to smuggle `jobject` through `Long`
    /// now promote them to `VTAG_OBJECT` via `coerce_value_for_return` before
    /// the value ever reaches a local. This test guards against the Spring
    /// Boot SEGV regression where pointer-shaped long bits were mis-rooted.
    #[test]
    fn scan_local_objects_does_not_root_long_with_pointer_shaped_bits() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        let fake = 0x1000usize as i64;
        frame.set_local_unchecked(0, Value::Long(fake));

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots, &heap);
        assert!(roots.is_empty(), "VTAG_LONG must never produce a root");
    }

    #[test]
    fn scan_local_objects_skips_long_that_is_not_aligned_object_pattern() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        frame.set_local_unchecked(0, Value::Long(7));

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots, &heap);
        assert!(roots.is_empty());
    }

    /// Symmetric with `scan_local_objects`: `VTAG_LONG` slots are primitives,
    /// GC must NOT remap them even if their bits look like a moved pointer.
    #[test]
    fn update_local_refs_does_not_touch_long_with_pointer_shaped_bits() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;
        use std::collections::HashMap;

        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        let old = 0x2000usize as i64;
        frame.set_local_unchecked(0, Value::Long(old));

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x2000usize, 0x3000usize);
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        frame.update_local_refs(&map, &heap);

        // Long primitive must be preserved verbatim. Use the compact
        // accessor (CompactValue's Long/Double tag ambiguity is resolved
        // by the caller's instruction context).
        assert_eq!(frame.get_local_compact(0).as_long(), Some(0x2000));
    }

    /// REGRESSION (bc math-ec collision-long corruption): a primitive `long`
    /// whose NaN-boxed bits *collide* with the SUB_OBJECT pattern AND whose
    /// 47-bit payload coincides with a live heap object's address must NOT be
    /// treated as a GC root. Before the `local_kinds` gate, the collision
    /// long was bit-identical to a genuine reference, so `scan_local_objects`
    /// rooted it and the moving collector then relocated + corrupted it. This
    /// is the discriminating case the older `…pointer_shaped_bits` test misses
    /// (a low-address long is not even `is_object()`).
    #[test]
    fn scan_local_objects_skips_collision_long_aliasing_live_object() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        // A real, live heap object — its address passes `is_heap_addr`.
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as u64;
        assert_eq!(addr & 0x7, 0, "heap objects are 8-byte aligned");
        assert!(addr < (1 << 47), "user-space heap addr fits in 47 bits");

        // A BC-F2m-style primitive `long` whose bits collide with SUB_OBJECT
        // and whose payload equals the live object's address — bit-identical
        // to `CompactValue::object(addr)`, but it is a *value*, not a pointer.
        let collision_bits = CompactValue::object(addr).raw_bits();

        // Bytecode that READS both slots at pc 0 so the per-bci liveness
        // filter (which correctly drops never-read-again slots) keeps them:
        // this test is about tag-collision semantics, not liveness.
        //   0: lload_0 ; 1: pop2 ; 2: aload_1 ; 3: pop ; 4: return
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x1e, 0x58, 0x2b, 0x57, 0xb1],
            vec![],
            10,
            4,
            &[],
        );
        // Slot 0: stored as a long ⇒ LKIND_LONG ⇒ never a root.
        frame.set_local_unchecked(0, Value::Long(collision_bits as i64));
        // Slot 1: a genuine reference at the *same* address ⇒ a real root.
        frame.set_local_unchecked(1, Value::Object(Some(obj)));
        // Both slots carry identical SUB_OBJECT bits…
        assert!(frame.get_local_compact(0).is_object());
        assert_eq!(frame.get_local_compact(0).as_object_ptr(), Some(addr));

        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots, &heap);

        // …yet only the reference slot is rooted; the collision long is not.
        assert_eq!(roots.len(), 1, "only the genuine reference is a GC root");
        assert_eq!(roots[0].as_ptr() as u64, addr);
    }


    /// gcj j2/pn2 (`CRATONVM_GEN_INTERP_LOCALS_TYPE_MAP`): a live local the
    /// frame's type map calls an `int` at its pc is not probed, even when its
    /// bits name a live object (the positive control: `=0` probes and roots
    /// it); no analysis (`None`) keeps every slot a candidate. The primitive-
    /// kind switch is held off so only the type map decides.
/// A `double` whose raw bits collide with the NaN-box tag space round-trips
    /// through a local slot bit-exact, and is never a GC root even when its
    /// 47-bit payload is a live object's address.
    ///
    /// The sibling of `scan_local_objects_skips_collision_long_aliasing_live_object`
    /// for the other category-2 primitive. `CompactValue::long` has stored
    /// verbatim since the BC SM2 fix (2026-05-28) and the `local_kinds` gate is
    /// what makes that safe; doubles now store verbatim through
    /// `CompactValue::double_raw` for the same reason, which is what keeps a NaN
    /// payload alive across `dstore` / `dload` (see
    /// `nan-payloads-lost-to-the-compactvalue-tag-collision-FIXED-20260828`).
    #[test]
    fn tag_colliding_double_local_round_trips_and_is_never_a_root() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as u64;
        // Bit-identical to a genuine reference — but stored as a double.
        let collision_bits = CompactValue::object(addr).raw_bits();

        //   0: dload_0 ; 1: pop2 ; 2: aload_2 ; 3: pop ; 4: return
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x26, 0x58, 0x2c, 0x57, 0xb1],
            vec![],
            10,
            4,
            &[],
        );
        frame.set_local_unchecked(0, Value::Double(f64::from_bits(collision_bits)));
        frame.set_local_unchecked(2, Value::Object(Some(obj)));

        // The payload survived the store…
        assert_eq!(frame.get_local_raw(0), collision_bits);
        match frame.get_local(0) {
            Value::Double(d) => assert_eq!(d.to_bits(), collision_bits),
            other => panic!("a double local decoded as {other:?}"),
        }
        match frame.get_local_unchecked(0) {
            Value::Double(d) => assert_eq!(d.to_bits(), collision_bits),
            other => panic!("a double local decoded as {other:?}"),
        }
        // …and the slot really does carry the SUB_OBJECT bit pattern.
        assert!(frame.get_local_compact(0).is_object());
        assert_eq!(frame.get_local_compact(0).as_object_ptr(), Some(addr));

        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots, &heap);
        assert_eq!(roots.len(), 1, "only the genuine reference is a GC root");
        assert_eq!(roots[0].as_ptr() as u64, addr);

        // A moving collector must not rewrite the double either.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(addr as usize, (addr + 0x1000) as usize);
        frame.update_local_refs(&map, &heap);
        assert_eq!(
            frame.get_local_raw(0),
            collision_bits,
            "relocation must not touch a LKIND_DOUBLE slot",
        );
    }

    /// An argument passed by value keeps a tag-colliding NaN payload across the
    /// call boundary — `copy_args_to_locals` writes the LKIND_DOUBLE mark, so it
    /// can store the bits verbatim.
    #[test]
    fn tag_colliding_double_argument_survives_the_call_boundary() {
        let bits: u64 = 0xFFFE_5E0E_8000_0000;
        let frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "(D)V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            4,
            &[Value::Double(f64::from_bits(bits))],
        );
        assert_eq!(frame.get_local_raw(0), bits);
        match frame.get_local(0) {
            Value::Double(d) => assert_eq!(d.to_bits(), bits),
            other => panic!("a double argument decoded as {other:?}"),
        }
    }

    /// A category-2 (`long`/`double`) store must INVALIDATE the reserved upper
    /// half so a former object reference in that slot is not rooted forever.
    /// This is the interpreter-side root of the SteadyChurn unbounded-retention
    /// leak: `main`'s setup-loop `Node` temp lived in the slot pair the
    /// churn-loop `long` reused, so without clearing the upper half the dead
    /// node (and its whole `next` chain) stayed reachable.
    #[test]
    fn cat2_store_invalidates_reserved_upper_half() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);

        // Bytecode that reads slots 2 and 3 as a long at pc 0 so per-bci
        // liveness keeps the pair live — the test is about the store clearing
        // the upper half, not liveness dropping it.
        //   0: lload_2 ; 1: pop2 ; 2: return
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x20, 0x58, 0xb1],
            vec![],
            10,
            6,
            &[],
        );

        // Slot 3 first holds a live object (the "scoped-out temp").
        frame.set_local_unchecked(3, Value::Object(Some(obj)));
        // A long stored at slot 2 reserves slots 2 AND 3 — slot 3's stale
        // object reference must be gone.
        frame.set_local_unchecked(2, Value::Long(0x1_0000_0002));

        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots, &heap);
        assert!(
            roots.is_empty(),
            "cat-2 store did not clear the reserved upper half (stale object rooted)"
        );
        // The long still reads back intact from slot 2.
        assert_eq!(frame.get_local_compact(2).as_long(), Some(0x1_0000_0002));
    }

    /// `scan_local_objects` must NOT root a genuinely scoped-out OBJECT local
    /// (written once, never read again) — the general liveness imprecision,
    /// independent of the cat-2-store leak. Verifies the frame-level WIRING of
    /// the per-bci liveness filter (the analyzer itself is unit-tested in
    /// `runtime::local_liveness`).
    #[test]
    fn scan_local_objects_drops_scoped_out_object_local() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);

        // Bytecode: slot 1 written at pc 0, then an endless counter loop over
        // slot 2 that never reads slot 1.
        //   0: astore_1 ; 1: iinc 2,1 ; 4: goto 1
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x4c, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xfd],
            vec![],
            10,
            4,
            &[],
        );
        frame.set_local_unchecked(1, Value::Object(Some(obj)));
        // Position the frame in the loop (pc 1), where slot 1 is dead.
        frame.pc = 1;
        frame.last_instr_pc = 1;

        // gci i10/np: the liveness filter alone is the `=0` arm of
        // `CRATONVM_GC_INTERP_TYPE_MAP_LOCALS`. At the loop head slot 1 is
        // still a reference on both incoming paths (the store and the back
        // edge), so HotSpot's interpreter type map -- the default arm -- keeps
        // it.
        let mut roots = Vec::new();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_INTERP_TYPE_MAP_LOCALS", Some("0"))],
            || frame.scan_local_objects(&mut roots, &heap),
        );
        assert!(
            roots.is_empty(),
            "a scoped-out object local was rooted (liveness filter not applied)"
        );
        let mut typed_roots = Vec::new();
        cratonvm_types::flags::with_thread_overrides(&[], || {
            frame.scan_local_objects(&mut typed_roots, &heap)
        });
        assert_eq!(
            typed_roots,
            vec![obj],
            "the default arm keeps a dead but reference-typed local (HotSpot's interpreter oop map)"
        );

        // With the kill-switch the historical behaviour returns: the slot is
        // rooted. (Cannot toggle the cached env flag here, so assert the
        // analyzer agrees the slot is dead — the wiring above proves the
        // filter consults it.)
        let mask = crate::runtime::local_liveness::live_locals_mask(
            &frame.code,
            frame.exception_table(),
            frame.max_locals,
            [frame.pc, frame.last_instr_pc],
        );
        assert!(!mask.is_live(1), "analyzer must report slot 1 dead");

        let mut all_live_roots = Vec::new();
        frame.scan_local_objects_all_live(&mut all_live_roots, &heap);
        assert_eq!(
            all_live_roots,
            vec![obj],
            "all-live snapshot scan must keep the scoped-out reference"
        );
    }

    /// gch h1/fr (`gcf-f1f-debugger-local-reads-...`, the parity half): with a
    /// debugger observing locals the scoped-out object local is a root, as
    /// HotSpot keeps it under `can_access_local_variables`; without one the
    /// scan is the liveness-filtered one.
    #[test]
    fn gch_h1fr_a_debugger_observed_frame_roots_its_dead_object_local() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        //   0: astore_1 ; 1: iinc 2,1 ; 4: goto 1  (slot 1 dead in the loop)
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x4c, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xfd],
            vec![],
            10,
            4,
            &[],
        );
        frame.set_local_unchecked(1, Value::Object(Some(obj)));
        frame.pc = 1;
        frame.last_instr_pc = 1;

        // gci i10/np: "the liveness-filtered scan" is the `=0` arm of
        // `CRATONVM_GC_INTERP_TYPE_MAP_LOCALS`; on the default arm slot 1 is
        // reference-typed at the loop head and a root without a debugger too.
        let mut unobserved = Vec::new();
        let mut filtered = Vec::new();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_INTERP_TYPE_MAP_LOCALS", Some("0"))],
            || {
                frame.scan_local_objects_observed(&mut unobserved, &heap, false);
                frame.scan_local_objects(&mut filtered, &heap);
            },
        );
        assert_eq!(unobserved, filtered, "no debugger: the liveness-filtered scan");
        assert!(unobserved.is_empty(), "no debugger: the dead local is not a root");
        let mut typed = Vec::new();
        cratonvm_types::flags::with_thread_overrides(&[], || {
            frame.scan_local_objects_observed(&mut typed, &heap, false)
        });
        assert_eq!(typed, vec![obj], "no debugger, default arm: the type map keeps it");

        let mut observed = Vec::new();
        frame.scan_local_objects_observed(&mut observed, &heap, true);
        assert_eq!(observed, vec![obj], "a debugger: every local is a root");
    }

    /// gci i10/np (`gci-i10np-interpreted-frames-drop-liveness-dead-reference-locals-hotspot-keeps-FIXED-20261016`):
    /// the default interpreter root scan keeps a liveness-dead local that is
    /// reference-typed at the pc (HotSpot's `GenerateOopMap` answer), does NOT
    /// keep one whose type is a conflict at a join (a reference on one path,
    /// never written on the other: GenerateOopMap's ref/uninit merge), and
    /// publishes a dead typed slot only when it names an object start.
    ///   0: iload_1 ; 1: ifeq -> 7 ; 4: aload_2 ; 5: astore_3 ; 6: nop ; 7: return
    #[test]
    fn gci_i10np_a_dead_typed_local_is_a_root_and_a_join_conflict_is_not() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x1b, 0x99, 0x00, 0x06, 0x2c, 0x4e, 0x00, 0xb1],
            vec![],
            10,
            4,
            &[],
        );
        frame.set_local_unchecked(3, Value::Object(Some(obj)));
        let scan = |frame: &Frame, kill: bool| {
            let mut roots = Vec::new();
            let edits: &[(&str, Option<&str>)] = if kill {
                &[("CRATONVM_GC_INTERP_TYPE_MAP_LOCALS", Some("0"))]
            } else {
                &[]
            };
            cratonvm_types::flags::with_thread_overrides(edits, || {
                frame.scan_local_objects(&mut roots, &heap)
            });
            roots
        };

        // pc 6: only the storing path reaches it -- dead, but a reference.
        frame.pc = 6;
        frame.last_instr_pc = 6;
        assert!(!frame.live_locals_mask_here().is_live(3), "fixture: slot 3 is dead at pc 6");
        assert_eq!(scan(&frame, false), vec![obj], "default: the typed dead slot is a root");
        assert!(scan(&frame, true).is_empty(), "`=0`: liveness alone drops it");

        // pc 7: the join of the storing path and the branch that skipped the
        // store -- a conflict, not a reference, on HotSpot as here.
        frame.pc = 7;
        frame.last_instr_pc = 7;
        assert!(scan(&frame, false).is_empty(), "a ref/uninit join is not a root");

        // A dead typed slot that names no object start is not published.
        // SAFETY: never dereferenced; the scan only classifies the address.
        let stale = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };
        frame.set_local_unchecked(3, Value::Object(Some(stale)));
        frame.pc = 6;
        frame.last_instr_pc = 6;
        assert!(scan(&frame, false).is_empty(), "a stale word in a dead typed slot is screened");
    }

    /// `r19-zgc-mark-skips-wild-pointers-in-compatible-ffm-in-native-battery-FIXED-20261007`:
    /// on ZGC the all-live scan keeps a DEAD slot only when it still names an
    /// object start. A dead slot holding an address inside an object (its old
    /// object swept, the memory reused) is what the marker counted as
    /// `wild_skipped`; a dead slot naming a real object is kept as before, and
    /// a live slot is untouched.
    #[test]
    fn all_live_scan_keeps_a_dead_slot_only_at_an_object_start_on_zgc() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Zgc, 16 * 1024 * 1024);
        assert!(heap.object_start_is_the_mark_screen());
        let obj = heap.alloc_object(ClassId::new(0), 4);
        let interior = obj.as_ptr() as usize + 8;
        assert!(
            heap.is_heap_addr(interior).is_some(),
            "the slot filter admits the interior word, so only the new screen drops it"
        );
        assert!(heap.is_object_address(interior).is_none());

        // 0: astore_1 ; 1: iinc 2,1 ; 4: goto 1 -- at pc 1 slots 1 and 3 are dead.
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x4c, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xfd],
            vec![],
            10,
            4,
            &[],
        );
        frame.set_local_unchecked(1, Value::Object(Some(obj)));
        // SAFETY: never dereferenced; the scan only classifies the address.
        let stale = unsafe { ObjectRef::from_raw(interior as *mut u8) };
        frame.set_local_unchecked(3, Value::Object(Some(stale)));
        frame.pc = 1;
        frame.last_instr_pc = 1;

        let mut roots = Vec::new();
        frame.scan_local_objects_all_live(&mut roots, &heap);
        assert_eq!(
            roots,
            vec![obj],
            "the dead interior word must not be published"
        );

        // Slot 0 is always live: an interior word there is left to the
        // existing filters, exactly as before.
        frame.set_local_unchecked(0, Value::Object(Some(stale)));
        let mut roots = Vec::new();
        frame.scan_local_objects_all_live(&mut roots, &heap);
        assert_eq!(roots, vec![stale, obj]);

        // Generational keeps the old all-live answer for the dead slots.
        let gen = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        assert!(!gen.object_start_is_the_mark_screen());
    }

    #[test]
    fn scan_local_objects_roots_lost_tag_other_local() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as u64;

        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );

        // Model a reference-typed local whose CompactValue object tag was lost:
        // the slot kind is still LKIND_OTHER, but the bits are a raw heap addr.
        frame.set_local_compact_unchecked(0, CompactValue::long(addr as i64));
        assert!(!frame.get_local_compact(0).is_object());

        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots, &heap);

        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].as_ptr() as u64, addr);
    }

    #[test]
    fn set_local_compact_long_preserves_kind_and_upper_half_mark() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );

        // `CompactValue::long(-1)` is a real compact-tagged `Long` collision.
        // Preserving the kind keeps this category-2 slot from being treated as a
        // stale upper half in later scans and OSR snapshots.
        let cv = CompactValue::long(-1);
        assert_eq!(cv.tag(), CompactTag::Long);
        frame.set_local_compact(1, CompactValue::object(0x1000));
        frame.set_local_compact(0, cv);

        assert_eq!(frame.get_local_tag(0), VTAG_LONG);
        assert_eq!(frame.get_local_raw(0), u64::MAX);
        assert_eq!(frame.get_local_tag(1), VTAG_LONG);
        assert_eq!(frame.get_local_raw(1), 0);
    }

    #[test]
    fn update_local_refs_remaps_lost_tag_other_local() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;
        use std::collections::HashMap;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let old_obj = heap.alloc_object(ClassId::new(0), 0);
        let new_obj = heap.alloc_object(ClassId::new(0), 0);
        let old_addr = old_obj.as_ptr() as u64;
        let new_addr = new_obj.as_ptr() as u64;
        assert_ne!(old_addr, new_addr);

        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        frame.set_local_compact_unchecked(0, CompactValue::long(old_addr as i64));
        assert!(!frame.get_local_compact(0).is_object());

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old_addr as usize, new_addr as usize);
        frame.update_local_refs(&map, &heap);

        let remapped = frame.get_local_compact(0);
        assert!(remapped.is_object());
        assert_eq!(remapped.as_object_ptr(), Some(new_addr));
    }

    /// Symmetric `update_local_refs` regression: a collision long whose payload
    /// equals some *unrelated* object's from-space address must be left
    /// verbatim even though that address is a `pointer_map` key (the map is
    /// global — rooting and remapping key off different objects).
    #[test]
    fn update_local_refs_preserves_collision_long_matching_pointer_map_key() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;
        use std::collections::HashMap;

        // Collision long: SUB_OBJECT bits with payload 0x2000.
        let collision_bits = CompactValue::object(0x2000).raw_bits();
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        frame.set_local_unchecked(0, Value::Long(collision_bits as i64));
        assert!(frame.get_local_compact(0).is_object());
        assert_eq!(frame.get_local_compact(0).as_object_ptr(), Some(0x2000));

        // A relocation of *some other* object that happened to live at 0x2000.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x2000usize, 0x3000usize);
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        frame.update_local_refs(&map, &heap);

        // The primitive long is preserved bit-exact (NOT rewritten to 0x3000).
        assert_eq!(
            frame.get_local_compact(0).as_long_unchecked(),
            collision_bits as i64
        );
    }

    /// i1 wave 5, lane L4 — precise-oop-map shadow, stage 1: against a
    /// published verifier map, a reference local whose value the heuristic
    /// scan cannot root is `Missed`, and a rooted local the map types `int` is
    /// `Extra`.
    #[test]
    fn oop_map_shadow_reports_missed_and_extra_locals() {
        use crate::memory::VmHeap;
        use crate::runtime::local_liveness::{oop_map_shadow, LiveMask};
        use cratonvm_classloading::vtype::VType;
        use cratonvm_gc::GcBackend;

        // A VM's own store (wave 7: the shadow no longer reads the process
        // mirror). Inside the store's 4,194,304 addressable ids: an id past it
        // is silently unpublished and the frame reads as unmapped.
        let cid = ClassId::new(0x003F_4C34);
        let mut builder = cratonvm_classloading::MethodTypeMapsBuilder::new(2, 1);
        builder.record_slots(
            0,
            &[VType::ObjectRef(Arc::from("java/lang/Object")), VType::Int],
            &[],
        );
        let maps = builder.finish(true);
        let store = cratonvm_classloading::TypeMapStore::new();
        assert!(store.publish(
            cid,
            cratonvm_classloading::ClassTypeMaps::new(vec![(
                Arc::from("m"),
                Arc::from("()V"),
                Some(maps),
            )]),
        ));

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let mut frame = Frame::new(
            cid,
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            1,
            2,
            &[],
        );
        // Slot 0: typed as a reference, holding an int — nothing the scan can root.
        frame.set_local_unchecked(0, Value::Int(12345));
        // Slot 1: typed `int`, holding a live object — rooted by its tag.
        frame.set_local_unchecked(1, Value::Object(Some(obj)));

        let [_, _, _, _, _, missed_before, extra_before] = oop_map_shadow::counts();
        frame.shadow_compare_oop_maps(&heap, &LiveMask::all_live(), &store);
        let [_, _, _, _, _, missed_after, extra_after] = oop_map_shadow::counts();
        assert!(missed_after > missed_before, "slot 0 is a missed reference");
        assert!(extra_after > extra_before, "slot 1 is an extra root");

        // The same frame against a store that never saw the class: unmapped,
        // no verdicts — the process store's maps are not consulted.
        let empty = cratonvm_classloading::TypeMapStore::new();
        let [_, unmapped_before, _, _, _, _, _] = oop_map_shadow::counts();
        frame.shadow_compare_oop_maps(&heap, &LiveMask::all_live(), &empty);
        let [_, unmapped_after, _, _, _, _, _] = oop_map_shadow::counts();
        assert!(unmapped_after > unmapped_before);
    }

    /// Interpreter round i6 wave 121, lane L7 (stage 3,
    /// `CRATONVM_INTERP_PRECISE_OOP_MAPS`): the slot rule. Only a slot every
    /// mapped pc calls a non-reference is left out, and never a `long` /
    /// `double`-marked one (its minted-handle root is the scan's own rule).
    #[test]
    fn the_precise_rule_leaves_out_only_unmarked_non_reference_slots() {
        use crate::runtime::local_liveness::oop_map_shadow::MapAnswer;
        assert!(precise_map_excludes_local(MapAnswer::NotReference, false));
        assert!(!precise_map_excludes_local(MapAnswer::NotReference, true));
        assert!(!precise_map_excludes_local(MapAnswer::Reference, false));
        assert!(!precise_map_excludes_local(MapAnswer::Reference, true));
        assert!(!precise_map_excludes_local(MapAnswer::Unknown, false));
        assert!(!precise_map_excludes_local(MapAnswer::Unknown, true));
        // The fold over the two pcs: one reference row keeps the slot.
        assert!(!precise_map_excludes_local(
            MapAnswer::from_rows([false, true].into_iter()),
            false
        ));
        assert!(precise_map_excludes_local(
            MapAnswer::from_rows([false, false].into_iter()),
            false
        ));
    }

    /// Interpreter round i6 wave 121, lane L7: the precise scan against a
    /// published map. Slot 0 is reference-typed and holds an object (rooted in
    /// both arms); slot 1 is `int`-typed and holds an object (rooted by its
    /// tag off, left out on). A store with no row for the method keeps the
    /// heuristic whole even with the switch on.
    #[test]
    fn the_precise_scan_leaves_out_what_the_map_calls_non_references() {
        use crate::memory::VmHeap;
        use cratonvm_classloading::vtype::VType;
        use cratonvm_gc::GcBackend;

        let cid = ClassId::new(0x003F_4C35);
        let mut builder = cratonvm_classloading::MethodTypeMapsBuilder::new(2, 1);
        builder.record_slots(
            0,
            &[VType::ObjectRef(Arc::from("java/lang/Object")), VType::Int],
            &[],
        );
        let store = cratonvm_classloading::TypeMapStore::new();
        assert!(store.publish(
            cid,
            cratonvm_classloading::ClassTypeMaps::new(vec![(
                Arc::from("m"),
                Arc::from("()V"),
                Some(builder.finish(true)),
            )]),
        ));
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let a = heap.alloc_object(ClassId::new(0), 0);
        let b = heap.alloc_object(ClassId::new(0), 0);
        // aload_0; pop; iload_1; pop; return: both slots are read after pc 0,
        // so both are live there and the liveness filter keeps both.
        let mut frame = Frame::new(
            cid,
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x2a, 0x57, 0x1b, 0x57, 0xb1],
            vec![],
            1,
            2,
            &[],
        );
        frame.set_local_unchecked(0, Value::Object(Some(a)));
        frame.set_local_unchecked(1, Value::Object(Some(b)));

        let scan = |store: &cratonvm_classloading::TypeMapStore| {
            let mut roots = Vec::new();
            frame.scan_local_objects_mapped(&mut roots, &heap, store);
            roots
        };
        store.set_precise_scan(false);
        let off = scan(&store);
        assert!(off.contains(&a) && off.contains(&b), "off: the heuristic roots both");
        store.set_precise_scan(true);
        let on = scan(&store);
        assert!(on.contains(&a), "on: the reference-typed slot stays a root");
        assert!(!on.contains(&b), "on: the int-typed slot is left out");
        assert_eq!(
            frame.precise_excluded_locals(&store),
            Some(vec![false, true]),
            "the remap reads the same set"
        );
        let empty = cratonvm_classloading::TypeMapStore::new();
        empty.set_precise_scan(true);
        let unmapped = scan(&empty);
        assert!(
            unmapped.contains(&a) && unmapped.contains(&b),
            "no row: the heuristic whole"
        );
        assert_eq!(frame.precise_excluded_locals(&empty), None);
    }

    /// Wave 124 (interpreter round i1 closure, lane L4): the precise scan must
    /// not drop what HotSpot's interpreter oop map keeps. javac drops a
    /// scoped-out local from the next StackMapTable frame, so the verifier's
    /// map calls it `Top` while `GenerateOopMap` (gci i10/np's type half) still
    /// types it a reference: the switch leaves the dead typed slot a root, and
    /// still leaves out one whose type is a join conflict.
    ///   0: iload_1 ; 1: ifeq -> 7 ; 4: aload_2 ; 5: astore_3 ; 6: nop ; 7: return
    #[test]
    fn i1_l4_precise_scan_keeps_a_dead_typed_local_hotspot_keeps() {
        use crate::memory::VmHeap;
        use cratonvm_classloading::vtype::VType;
        use cratonvm_gc::GcBackend;

        let cid = ClassId::new(0x003F_4C37);
        let mut builder = cratonvm_classloading::MethodTypeMapsBuilder::new(4, 1);
        let tops = [VType::Top, VType::Top, VType::Top, VType::Top];
        builder.record_slots(6, &tops, &[]);
        builder.record_slots(7, &tops, &[]);
        let store = cratonvm_classloading::TypeMapStore::new();
        assert!(store.publish(
            cid,
            cratonvm_classloading::ClassTypeMaps::new(vec![(
                Arc::from("m"),
                Arc::from("()V"),
                Some(builder.finish(true)),
            )]),
        ));
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let mut frame = Frame::new(
            cid,
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x1b, 0x99, 0x00, 0x06, 0x2c, 0x4e, 0x00, 0xb1],
            vec![],
            10,
            4,
            &[],
        );
        frame.set_local_unchecked(3, Value::Object(Some(obj)));
        let scan = |frame: &Frame| {
            let mut roots = Vec::new();
            frame.scan_local_objects_mapped(&mut roots, &heap, &store);
            roots
        };

        // pc 6: only the storing path reaches it -- dead, typed a reference by
        // GenerateOopMap, `Top` in the verifier's map.
        frame.pc = 6;
        frame.last_instr_pc = 6;
        assert!(!frame.live_locals_mask_here().is_live(3), "fixture: slot 3 is dead at pc 6");
        store.set_precise_scan(false);
        assert_eq!(scan(&frame), vec![obj], "off: the typed dead slot is a root");
        store.set_precise_scan(true);
        assert_eq!(
            frame.precise_excluded_locals(&store).map(|v| v[3]),
            Some(true),
            "fixture: the map calls slot 3 a non-reference, so only the HotSpot rule keeps it"
        );
        assert_eq!(scan(&frame), vec![obj], "on: HotSpot keeps it, so does the precise scan");

        // The shadow reads the effective set: kept on purpose, not an `extra`
        // and not a drop.
        let [_, retained_before] = crate::runtime::local_liveness::oop_map_shadow::parity_counts();
        frame.shadow_compare_oop_maps(&heap, &frame.live_locals_mask_here(), &store);
        let [_, retained_after] = crate::runtime::local_liveness::oop_map_shadow::parity_counts();
        assert!(retained_after > retained_before, "the retained slot is counted");

        // pc 7: a ref/uninit join is not a reference on HotSpot, and the map
        // calls it non-reference too: the precise scan leaves it out as the
        // heuristic does.
        frame.pc = 7;
        frame.last_instr_pc = 7;
        assert!(scan(&frame).is_empty(), "on: a join conflict is not a root");
        store.set_precise_scan(false);
        assert!(scan(&frame).is_empty(), "off: nor is it one");
    }

    /// Wave 124: with the switch on the shadow reads the precise scan's own
    /// roots, so a slot the map calls `int` that the heuristic roots is a
    /// `precise_dropped`, not an `extra`.
    #[test]
    fn i1_l4_shadow_counts_what_the_precise_scan_drops() {
        use crate::memory::VmHeap;
        use crate::runtime::local_liveness::{oop_map_shadow, LiveMask};
        use cratonvm_classloading::vtype::VType;
        use cratonvm_gc::GcBackend;

        let cid = ClassId::new(0x003F_4C38);
        let mut builder = cratonvm_classloading::MethodTypeMapsBuilder::new(2, 1);
        builder.record_slots(
            0,
            &[VType::ObjectRef(Arc::from("java/lang/Object")), VType::Int],
            &[],
        );
        let store = cratonvm_classloading::TypeMapStore::new();
        assert!(store.publish(
            cid,
            cratonvm_classloading::ClassTypeMaps::new(vec![(
                Arc::from("m"),
                Arc::from("()V"),
                Some(builder.finish(true)),
            )]),
        ));
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let mut frame = Frame::new(
            cid,
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            1,
            2,
            &[],
        );
        // Slot 1: typed `int`, holding a live object -- rooted by its tag.
        frame.set_local_unchecked(1, Value::Object(Some(obj)));

        // Other tests share the counters, so only the arm that must count
        // asserts, and only a lower bound.
        store.set_precise_scan(true);
        let [dropped_before, _] = oop_map_shadow::parity_counts();
        frame.shadow_compare_oop_maps(&heap, &LiveMask::all_live(), &store);
        let [dropped_after, _] = oop_map_shadow::parity_counts();
        assert!(dropped_after > dropped_before, "the int-typed slot is a precise drop");
    }

    /// Interpreter round i6 wave 122, lane L7 (the stack half of
    /// `CRATONVM_INTERP_PRECISE_OOP_MAPS`): with the row's depth equal to the
    /// runtime depth, an operand-stack slot the row types `int` is left out;
    /// at another depth (a frame between two rows) the heuristic scans all.
    #[test]
    fn the_precise_stack_scan_reads_only_a_depth_matched_row() {
        use crate::memory::VmHeap;
        use cratonvm_classloading::vtype::VType;
        use cratonvm_gc::GcBackend;

        let cid = ClassId::new(0x003F_4C36);
        let mut builder = cratonvm_classloading::MethodTypeMapsBuilder::new(1, 2);
        builder.record_slots(
            0,
            &[VType::Int],
            &[VType::ObjectRef(Arc::from("java/lang/Object")), VType::Int],
        );
        let store = cratonvm_classloading::TypeMapStore::new();
        assert!(store.publish(
            cid,
            cratonvm_classloading::ClassTypeMaps::new(vec![(
                Arc::from("m"),
                Arc::from("()V"),
                Some(builder.finish(true)),
            )]),
        ));
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let a = heap.alloc_object(ClassId::new(0), 0);
        let b = heap.alloc_object(ClassId::new(0), 0);
        // pop; pop; return (the bytes only need a pc 0).
        let mut frame = Frame::new(
            cid,
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x57, 0x57, 0xb1],
            vec![],
            2,
            1,
            &[],
        );
        frame.stack.push(Value::Object(Some(a))).expect("push");
        frame.stack.push(Value::Object(Some(b))).expect("push");

        let scan = |frame: &Frame, store: &cratonvm_classloading::TypeMapStore| {
            let mut roots = Vec::new();
            frame.scan_stack_objects_mapped(&mut roots, &heap, true, Some(store));
            roots
        };
        store.set_precise_scan(false);
        let off = scan(&frame, &store);
        assert!(off.contains(&a) && off.contains(&b), "off: both stack slots");
        store.set_precise_scan(true);
        let on = scan(&frame, &store);
        assert!(on.contains(&a), "on: the reference-typed slot stays");
        assert!(!on.contains(&b), "on: the int-typed slot is left out");
        assert_eq!(frame.precise_excluded_stack(&store), Some(vec![false, true]));
        // Depth 1 against a depth-2 row: no row applies, the heuristic whole.
        let _ = frame.stack.pop();
        assert_eq!(frame.precise_excluded_stack(&store), None);
        let between = scan(&frame, &store);
        assert!(between.contains(&a));
    }

    /// i1 wave 7, lane L4 — the remap twin of the shadow: a reference-typed
    /// local that names a moved object but carries a `long` kind mark (the
    /// hazard `local_kind_at` documents) is left stale by the remap and is
    /// `Missed`; an `int`-typed slot holding an object the remap rewrote is
    /// `Extra`. The rewrite itself is exactly `update_local_refs` +
    /// `stack.update_object_refs`.
    #[test]
    fn oop_map_remap_shadow_reports_missed_and_extra_locals() {
        use crate::memory::VmHeap;
        use crate::runtime::local_liveness::oop_map_shadow;
        use cratonvm_classloading::vtype::VType;
        use cratonvm_gc::GcBackend;

        let cid = ClassId::new(0x0000_0107);
        let mut builder = cratonvm_classloading::MethodTypeMapsBuilder::new(2, 1);
        builder.record_slots(
            0,
            &[VType::ObjectRef(Arc::from("java/lang/Object")), VType::Int],
            &[],
        );
        let store = cratonvm_classloading::TypeMapStore::new();
        assert!(store.publish(
            cid,
            cratonvm_classloading::ClassTypeMaps::new(vec![(
                Arc::from("m"),
                Arc::from("()V"),
                Some(builder.finish(true)),
            )]),
        ));

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        // The minted-handle tables are keyed by the heap's address (a stack slot
        // here): drop any entry an earlier test on this thread left at it, or a
        // stale mint equal to `old_addr` makes the `long` slot below remappable.
        crate::memory::smuggled_longs::forget_heap(&heap);
        let old_obj = heap.alloc_object(ClassId::new(0), 0);
        let new_obj = heap.alloc_object(ClassId::new(0), 0);
        let old_addr = old_obj.as_ptr() as usize;
        let new_addr = new_obj.as_ptr() as usize;
        // Slot 0 (map: reference): an `int` whose bits equal a moved object's
        // old address is not rewritten. Slot 1 (map: `int`): an object that is.
        let mut frame = Frame::new(
            cid,
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            // aload_0; iload_1; return — both slots live at pc 0.
            vec![0x2a, 0x1b, 0xb1],
            vec![],
            2,
            2,
            &[],
        );
        let lost = CompactValue::long(old_addr as i64);
        frame.set_local_compact_unchecked(0, lost);
        frame.set_local_unchecked(1, Value::Object(Some(old_obj)));
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old_addr, new_addr);
        // Without the mark, the lost-tag path would recover and rewrite slot
        // 0; with it the remap treats the slot as a primitive (unminted, so
        // verbatim) — the stale reference the precise map would have fixed.
        frame.local_kinds[0] = LKIND_LONG;

        let [_, missed_before, extra_before] = oop_map_shadow::remap_counts();
        let snapshot = frame.shadow_slot_snapshot();
        frame.update_local_refs(&map, &heap);
        frame.stack.update_object_refs(&map, &heap);
        frame.shadow_compare_remap(&map, &store, &snapshot.0, &snapshot.1);
        let [_, missed_after, extra_after] = oop_map_shadow::remap_counts();
        assert_eq!(
            frame.get_local_raw(0),
            lost.raw_bits(),
            "slot 0 is left alone"
        );
        assert_eq!(
            frame.get_local_compact(1).as_object_ptr(),
            Some(new_addr as u64),
            "slot 1 follows the move"
        );
        assert!(missed_after > missed_before, "slot 0 is a missed remap");
        assert!(extra_after > extra_before, "slot 1 is an extra rewrite");

        // The public entry rewrites exactly as the direct pair does.
        let mut twin = Frame::new(
            cid,
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x2a, 0x1b, 0xb1],
            vec![],
            2,
            2,
            &[],
        );
        twin.set_local_unchecked(1, Value::Object(Some(old_obj)));
        twin.update_frame_refs_mapped(&map, &heap, &store);
        assert_eq!(
            twin.get_local_compact(1).as_object_ptr(),
            Some(new_addr as u64)
        );
    }

    /// i1 wave 5, lane L4: a `long` local holding a MINTED smuggled handle is
    /// rooted and remapped, like the same value on the operand stack; an
    /// unminted `long` equal to a live object's address stays unrooted and
    /// verbatim.
    #[test]
    fn minted_long_local_is_rooted_and_remapped_unminted_is_not() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let minted = heap.alloc_object(ClassId::new(0), 0);
        let plain = heap.alloc_object(ClassId::new(0), 0);
        let moved_to = heap.alloc_object(ClassId::new(0), 0);
        let minted_addr = minted.as_ptr() as u64;
        let plain_addr = plain.as_ptr() as u64;
        let new_addr = moved_to.as_ptr() as u64;
        // The mint registry is keyed by the heap's address, which this local
        // shares with earlier tests' heaps on the thread: start clean.
        crate::memory::smuggled_longs::forget_heap(&heap);
        crate::memory::smuggled_longs::record_minted_long(&heap, minted_addr);

        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            4,
            &[],
        );
        frame.set_local_unchecked(0, Value::Long(minted_addr as i64));
        frame.set_local_unchecked(2, Value::Long(plain_addr as i64));

        let mut roots = Vec::new();
        frame.scan_local_objects_all_live(&mut roots, &heap);
        assert!(roots.iter().any(|r| r.as_ptr() as u64 == minted_addr));
        assert!(!roots.iter().any(|r| r.as_ptr() as u64 == plain_addr));

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(minted_addr as usize, new_addr as usize);
        map.insert(plain_addr as usize, new_addr as usize);
        frame.update_local_refs(&map, &heap);
        assert_eq!(
            frame.get_local_raw(0),
            new_addr,
            "minted handle follows the move"
        );
        assert_eq!(
            frame.get_local_raw(2),
            plain_addr,
            "a primitive stays verbatim"
        );
        crate::memory::smuggled_longs::forget_heap(&heap);
    }

    #[test]
    #[should_panic]
    fn get_local_unchecked_panics_out_of_bounds() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        let _ = frame.get_local_unchecked(99); // should panic
    }

    #[test]
    #[should_panic]
    fn set_local_unchecked_panics_out_of_bounds() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        frame.set_local_unchecked(99, Value::Int(1)); // should panic
    }

    #[test]
    fn get_local_out_of_bounds_returns_uninitialized() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        assert_eq!(frame.get_local(99), Value::Uninitialized);
    }

    #[test]
    fn set_local_out_of_bounds_is_no_op() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        frame.set_local(99, Value::Int(42)); // should not panic
        assert_eq!(frame.get_local(99), Value::Uninitialized);
    }

    #[test]
    fn frame_metadata_accessors() {
        let frame = Frame::new(
            ClassId::new(5),
            "com/example/Foo".to_string(),
            "bar".to_string(),
            "(I)V".to_string(),
            Some("Foo.java".to_string()),
            vec![0xb1], // return void
            vec![],
            10,
            3,
            &[],
        );
        assert_eq!(frame.class_name(), "com/example/Foo");
        assert_eq!(frame.method_name(), "bar");
        assert_eq!(frame.method_descriptor(), "(I)V");
        assert_eq!(frame.source_file(), Some("Foo.java"));
        assert_eq!(frame.class_id, ClassId::new(5));
    }

    #[test]
    fn padded_bytecode_adds_trailing_zeros() {
        let code = vec![0x2a, 0xb1]; // aload_0, return
        let padded = padded_bytecode(&code);
        assert_eq!(padded.len(), 4);
        assert_eq!(padded[0], 0x2a);
        assert_eq!(padded[1], 0xb1);
        assert_eq!(padded[2], 0);
        assert_eq!(padded[3], 0);
    }

    #[test]
    fn frame_recycle_returns_vecs() {
        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            5,
            &[Value::Int(1)],
        );
        frame.recycle(&mut locals_pool, &mut stacks_pool);
        assert_eq!(locals_pool.len(), 1);
        assert_eq!(stacks_pool.len(), 1);
    }

    #[test]
    #[should_panic]
    fn get_local_raw_panics_out_of_bounds() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        let _ = frame.get_local_raw(99); // should panic
    }

    #[test]
    #[should_panic]
    fn get_local_tag_panics_out_of_bounds() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        let _ = frame.get_local_tag(99); // should panic
    }

    // ── Additional edge case tests ────────────────────────────────────

    #[test]
    fn frame_creation_with_zero_locals() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            4,
            0, // zero locals
            &[],
        );
        assert_eq!(frame.locals_len(), 0);
        // get_local out of bounds returns Uninitialized
        assert_eq!(frame.get_local(0), Value::Uninitialized);
    }

    #[test]
    fn frame_creation_with_max_locals() {
        // Use a large max_locals value
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            u16::MAX, // 65535 locals
            &[],
        );
        assert_eq!(frame.locals_len(), u16::MAX as usize);
        // All locals should be uninitialized
        assert_eq!(frame.get_local(0), Value::Uninitialized);
        assert_eq!(frame.get_local(100), Value::Uninitialized);
        assert_eq!(frame.get_local(65534), Value::Uninitialized);
    }

    #[test]
    fn frame_local_set_get_boundary() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            3,
            &[],
        );
        // Set first and last local
        frame.set_local(0, Value::Int(100));
        frame.set_local(2, Value::Int(200));
        assert_eq!(frame.get_local(0).as_int(), Some(100));
        assert_eq!(frame.get_local(2).as_int(), Some(200));
        // Middle is still uninitialized
        assert_eq!(frame.get_local(1), Value::Uninitialized);

        // Out of bounds set is a no-op
        frame.set_local(3, Value::Int(300));
        assert_eq!(frame.get_local(3), Value::Uninitialized);
    }

    #[test]
    fn exception_table_lookup_matching() {
        let exception_table = vec![
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 10,
                handler_pc: 20,
                catch_type: 5, // specific exception class
            },
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 10,
                handler_pc: 30,
                catch_type: 0, // catch-all (finally)
            },
        ];
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            exception_table,
            4,
            2,
            &[],
        );
        let table = frame.exception_table();
        assert_eq!(table.len(), 2);
        // First entry covers pc 0..10, handler at 20
        assert_eq!(table[0].start_pc, 0);
        assert_eq!(table[0].end_pc, 10);
        assert_eq!(table[0].handler_pc, 20);
        assert_eq!(table[0].catch_type, 5);
    }

    #[test]
    fn exception_table_non_matching_range() {
        let exception_table = vec![ExceptionTableEntry {
            start_pc: 10,
            end_pc: 20,
            handler_pc: 30,
            catch_type: 0,
        }];
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            exception_table,
            4,
            2,
            &[],
        );
        let table = frame.exception_table();
        // PC 5 is outside [10, 20), so no handler matches
        let handler = table
            .iter()
            .find(|e| 5 >= e.start_pc as usize && 5 < e.end_pc as usize);
        assert!(handler.is_none());
    }

    #[test]
    fn exception_table_nested_handlers() {
        let exception_table = vec![
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 50,
                handler_pc: 100,
                catch_type: 0, // outer catch-all
            },
            ExceptionTableEntry {
                start_pc: 10,
                end_pc: 30,
                handler_pc: 60,
                catch_type: 5, // inner specific
            },
        ];
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            exception_table,
            4,
            2,
            &[],
        );
        let table = frame.exception_table();
        // PC 15 is inside both ranges
        let matches: Vec<_> = table
            .iter()
            .filter(|e| 15 >= e.start_pc as usize && 15 < e.end_pc as usize)
            .collect();
        assert_eq!(matches.len(), 2);
        // First match is the outer handler, second is inner
        assert_eq!(matches[0].handler_pc, 100);
        assert_eq!(matches[1].handler_pc, 60);
    }

    #[test]
    fn frame_with_no_exception_handlers() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![], // no exception handlers
            4,
            2,
            &[],
        );
        assert!(frame.exception_table().is_empty());
    }

    #[test]
    fn frame_source_file_none() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None, // no source file
            vec![],
            vec![],
            4,
            2,
            &[],
        );
        assert!(frame.source_file().is_none());
        assert!(frame.source_file_arc().is_none());
    }

    #[test]
    fn frame_pc_starts_at_zero() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0x2a, 0xb1],
            vec![],
            4,
            2,
            &[],
        );
        assert_eq!(frame.pc, 0);
        assert_eq!(frame.backward_count, 0);
    }

    #[test]
    fn background_osr_pending_restarts_stride_without_spending_rejection() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "hotLoop".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            1,
            1,
            &[],
        );
        frame.backward_count = 10_000;
        frame.osr_attempt_counts.push((4, 2));

        frame.record_osr_background_pending();

        assert_eq!(frame.backward_count, 0);
        assert_eq!(frame.osr_attempt_counts, vec![(4, 2)]);
    }

    /// An entry opened at 0 reads as "asked" without spending an attempt, and
    /// one set to `OSR_MAX_ATTEMPTS` retires the loop for the activation.
    #[test]
    fn osr_attempt_entries_open_at_zero_and_retire_at_the_cap() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "hotLoop".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            1,
            1,
            &[],
        );
        frame.backward_count = OSR_THRESHOLD_FOR_TEST;
        assert_eq!(frame.osr_attempts_at(4), None);
        frame.set_osr_attempts(4, 0);
        assert_eq!(frame.osr_attempts_at(4), Some(0));
        assert!(frame.should_try_osr(4, OSR_THRESHOLD_FOR_TEST));
        frame.record_osr_rejection(4);
        assert_eq!(frame.osr_attempts_at(4), Some(1));
        frame.set_osr_attempts(4, OSR_MAX_ATTEMPTS);
        assert_eq!(frame.osr_attempt_counts, vec![(4, OSR_MAX_ATTEMPTS)]);
        assert!(!frame.should_try_osr(4, OSR_THRESHOLD_FOR_TEST));
        assert_eq!(
            frame.osr_attempts_at(9),
            None,
            "other loop headers untouched"
        );
    }

    const OSR_THRESHOLD_FOR_TEST: u32 = 1_000;

    // ===================================================================
    // FrameStack — address stability
    // ===================================================================

    fn test_frame(name: &str, max_locals: u16) -> Frame {
        Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            name.to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            2,
            max_locals,
            &[],
        )
    }

    /// The core guarantee: after `reserve_stable(n)`, `n` pushes move nothing.
    /// This is what lets the interpreter hoist a `*mut Frame` across the
    /// dispatch loop instead of re-indexing `thread.frames[frame_idx]`.
    #[test]
    fn frame_addresses_are_stable_across_a_reserved_deep_push_sequence() {
        const DEPTH: usize = 4096;
        let mut frames = FrameStack::new();
        frames.reserve_stable(DEPTH);
        let epoch_after_reserve = frames.reloc_epoch();
        assert!(frames.stable_headroom() >= DEPTH);

        let mut addrs = Vec::with_capacity(DEPTH);
        for i in 0..DEPTH {
            frames.push(test_frame("deep", 1));
            addrs.push(&frames[i] as *const Frame);
            // Nothing already on the stack may have moved.
            assert_eq!(
                frames.reloc_epoch(),
                epoch_after_reserve,
                "push #{} relocated despite reserve_stable({})",
                i,
                DEPTH
            );
        }

        for (i, &expected) in addrs.iter().enumerate() {
            assert_eq!(
                &frames[i] as *const Frame, expected,
                "frame {i} moved during a reserved push sequence"
            );
        }

        // Popping never moves a survivor either.
        for _ in 0..DEPTH / 2 {
            frames.pop();
        }
        for (i, &expected) in addrs.iter().enumerate().take(DEPTH / 2) {
            assert_eq!(
                &frames[i] as *const Frame, expected,
                "frame {i} moved during pops"
            );
        }
    }

    /// Interpreter round i1 wave 26, lane L7: the dispatch loop polls its OS
    /// thread's poll word, not the stack's move count, so a move must bump
    /// both -- the stack's count (which the loop's slow path compares) and the
    /// word of the thread it runs on (which sends the loop there), by whole
    /// `MOVE_STEP`s that never touch the pause count.
    #[test]
    fn a_move_bumps_the_stack_count_and_this_threads_poll_word() {
        use crate::threading::gc_barrier::{GcBarrier, LoopPollWord};
        let barrier = GcBarrier::new();
        let word = barrier.loop_poll_word_for_this_thread();
        // SAFETY: this OS thread's thread-local word, alive for the whole test.
        let before = unsafe { &*word }.load();
        let mut frames = FrameStack::new();
        assert_eq!(frames.code_moves(), 0);
        frames.note_code_moved();
        frames.note_conversion_deferred(7);
        assert_eq!(frames.code_moves(), 2);
        assert!(frames.conversion_pending());
        // SAFETY: as above.
        let after = unsafe { &*word }.load();
        assert_eq!(after.wrapping_sub(before), 2 * LoopPollWord::MOVE_STEP);
        assert_eq!(
            after & LoopPollWord::PAUSES,
            0,
            "a move never counts a pause"
        );
        assert_eq!(
            barrier.loop_poll_word_for_this_thread(),
            word,
            "one word per OS thread"
        );
    }

    /// Interpreter round i1 wave 41, lane L3: a stack that never gave a
    /// conversion up defers one owed at redefinition count 0 -- the count a
    /// redefinition fence of the process's first redefinition is raised at.
    /// `0` used to mean "never gave up", so that deferral was refused and the
    /// fenced loop ran on across the swap. A give-up at a count still refuses
    /// a later deferral at that count, and only at it.
    #[test]
    fn a_fresh_stack_defers_a_conversion_owed_at_count_zero() {
        let mut frames = FrameStack::new();
        assert!(
            frames.note_conversion_deferred(0),
            "count 0 is not a give-up"
        );
        assert!(frames.conversion_pending());
        frames.give_up_conversion(0);
        assert!(!frames.conversion_pending());
        assert!(!frames.note_conversion_deferred(0), "given up at 0");
        assert!(!frames.conversion_pending());
        assert!(frames.note_conversion_deferred(1));
        let mut from_vec = FrameStack::from(Vec::<Frame>::new());
        assert!(
            from_vec.note_conversion_deferred(0),
            "the Vec conversion starts fresh too"
        );
    }

    /// The raw-pointer API must observe the same address as the safe one, and
    /// stay valid across pushes *and* pops of frames above it.
    #[test]
    fn frame_ptr_stays_valid_across_pushes_and_pops() {
        let mut frames = FrameStack::new();
        frames.reserve_stable(64);
        frames.push(test_frame("bottom", 3));

        let epoch = frames.reloc_epoch();
        let bottom = frames.frame_ptr(0);
        assert!(!bottom.is_null());
        assert_eq!(bottom as *const Frame, &frames[0] as *const Frame);
        assert_eq!(frames.current_ptr(), bottom, "depth 1: top == bottom");

        // SAFETY: index 0 is live, epoch unchanged, no other reference to it.
        unsafe { (*bottom).pc = 0x1234 };

        for _ in 0..48 {
            frames.push(test_frame("callee", 1));
        }
        assert_eq!(frames.reloc_epoch(), epoch, "reserved pushes must not grow");
        while frames.len() > 1 {
            frames.pop();
        }

        assert_eq!(frames.frame_ptr(0), bottom, "bottom frame moved");
        // SAFETY: same conditions as above.
        assert_eq!(
            unsafe { (*bottom).pc },
            0x1234,
            "bottom frame was clobbered"
        );

        // Out of range yields null rather than panicking.
        assert!(frames.frame_ptr(1).is_null());
        frames.pop();
        assert!(frames.current_ptr().is_null());
    }

    /// Growth is the ONLY thing that moves frames, and it is always announced
    /// via `reloc_epoch`.
    #[test]
    fn growth_is_the_only_relocation_and_always_bumps_the_epoch() {
        let mut frames = FrameStack::new();
        assert_eq!(frames.reloc_epoch(), 0);

        // First push allocates, but there is nothing to move → no bump.
        frames.push(test_frame("first", 1));
        assert_eq!(frames.reloc_epoch(), 0, "initial reservation moved nothing");
        assert!(frames.stable_headroom() >= FRAME_STACK_INITIAL_STABLE_CAP - 1);

        // Fill exactly to capacity: still no relocation.
        while frames.stable_headroom() > 0 {
            frames.push(test_frame("fill", 1));
        }
        assert_eq!(frames.reloc_epoch(), 0);

        // The next push must grow, which must bump the epoch. (Addresses taken
        // before this point are stale by construction — that is precisely what
        // the epoch bump reports — so we assert the epoch and never dereference
        // a pre-growth pointer.)
        frames.push(test_frame("overflow", 1));
        assert_eq!(frames.reloc_epoch(), 1, "growth must bump reloc_epoch");
        // Capacity at least doubled, so a long run of pushes is stable again.
        assert!(frames.stable_headroom() >= FRAME_STACK_INITIAL_STABLE_CAP - 1);

        // truncate/clear keep the (now larger) stable capacity and move nothing.
        let cap = frames.stable_headroom() + frames.len();
        frames.truncate(10);
        assert_eq!(frames.reloc_epoch(), 1);
        assert_eq!(frames.stable_headroom() + frames.len(), cap);
        frames.clear();
        assert!(frames.is_empty());
        assert_eq!(frames.stable_headroom(), cap);
    }

    /// The drop-in surface the rest of the VM depends on (`Deref` to
    /// `&[Frame]` for `stackwalker::capture_full_trace`, iteration by
    /// reference, `Index`, `last`/`get`).
    #[test]
    fn frame_stack_is_a_drop_in_for_vec_frame() {
        let mut frames = FrameStack::new();
        for i in 0..4 {
            frames.push(test_frame(&format!("m{i}"), 1));
        }

        // Deref to a slice — what `capture_full_trace(&thread.frames)` needs.
        fn takes_slice(f: &[Frame]) -> usize {
            f.len()
        }
        assert_eq!(takes_slice(&frames), 4);
        assert_eq!(frames[..2].len(), 2);

        // Iteration in both directions, by shared and mutable reference.
        assert_eq!(frames.iter().count(), 4);
        assert_eq!(
            frames.iter().enumerate().rev().take(2).count(),
            2,
            "iter() must be DoubleEnded + ExactSize"
        );
        assert!(frames.iter().any(|f| f.method_name() == "m3"));
        for f in &frames {
            assert_eq!(f.class_name(), "Test");
        }
        for f in &mut frames {
            f.pc = 7;
        }
        assert!(frames.iter().all(|f| f.pc == 7));

        assert_eq!(frames.last().map(|f| f.method_name()), Some("m3"));
        frames.last_mut().expect("non-empty").pc = 9;
        assert_eq!(frames[3].pc, 9);
        assert!(frames.get(9).is_none());
        assert_eq!(frames.get_mut(0).map(|f| f.pc), Some(7));
        assert_eq!(
            frames.pop().map(|f| f.method_name().to_string()),
            Some("m3".to_string())
        );
        assert_eq!(frames.len(), 3);
    }

    // ===================================================================
    // FrameStack — slot-slab windows (wave 29, lane L7)
    // ===================================================================
    //
    // These call the slab arms directly (`emplace_*_in_window`), so they do
    // not depend on `CRATONVM_JIT_NO_LOCALS_SLAB`; a retired window is rebuilt
    // in a window whatever the flag says.

    /// The claim the slab's A/B rests on: a frame whose buffers are a window
    /// holds exactly what the pooled builders give the same call -- including
    /// a `long` and a `double` argument, whose upper slots are filler.
    #[test]
    fn a_windowed_frame_holds_what_a_pooled_frame_holds() {
        let cached = cached_probe("m", "(JIDLjava/lang/Object;)V", 9, 5);
        let compact = [
            (
                CompactValue::from_value_kinded(Value::Long(-9_000_000_000)),
                b'J',
            ),
            (CompactValue::from_value_kinded(Value::Int(7)), b'I'),
            (CompactValue::from_value_kinded(Value::Double(2.5)), b'D'),
            (CompactValue::null(), b'L'),
        ];
        let values = [
            Value::Long(-9_000_000_000),
            Value::Int(7),
            Value::Double(2.5),
            Value::Object(None),
        ];
        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();
        let pooled_compact = Frame::new_pooled_cached_compact(
            Arc::clone(&cached),
            &compact,
            &mut locals_pool,
            &mut stacks_pool,
        );
        let pooled_value = Frame::new_pooled_cached(
            Arc::clone(&cached),
            &values,
            &mut locals_pool,
            &mut stacks_pool,
        );

        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(Arc::clone(&cached), &compact);
        frames.emplace_value_in_window(Arc::clone(&cached), &values);
        assert_eq!(frames.len(), 2);
        for f in frames.iter() {
            assert!(f.locals.is_window() && f.local_kinds.is_window() && f.stack.is_windowed());
        }
        assert_eq!(frame_shape(&frames[0]), frame_shape(&pooled_compact));
        assert_eq!(frame_shape(&frames[1]), frame_shape(&pooled_value));
        assert_eq!(frames[0].stack.capacity(), pooled_compact.stack.capacity());
        assert_eq!(frames[0].get_local(2), Value::Int(7));
        assert_eq!(frames[0].get_local(3), Value::Double(2.5));
        // One window per frame: the operand stack follows the locals, and the
        // next frame's window follows that (addresses compared as integers).
        let slot = std::mem::size_of::<CompactValue>();
        let window = frames[0].locals_len() + frames[0].stack.capacity();
        assert_eq!(
            frames[1].locals.as_ptr() as usize,
            frames[0].locals.as_ptr() as usize + window * slot
        );
        let _ = frames[0].stack.push(Value::Int(41));
        assert_eq!(frames[0].stack.peek_int_unchecked(), 41);
        assert_eq!(
            frames[1].get_local(2),
            Value::Int(7),
            "a push stays in its own window"
        );
    }

    /// Recursion, the shape the slab is for: windows deep enough to span
    /// several slab chunks keep their contents and addresses while frames
    /// above them are pushed, retired (the interpreter's return), rebuilt in
    /// their retired slots, and truncated.
    #[test]
    fn windowed_frames_keep_their_locals_across_deeper_pushes_and_pops() {
        const DEPTH: usize = 3000;
        let cached = cached_probe("f", "(I)I", 3, 4);
        let mut frames = FrameStack::new();
        frames.reserve_stable(DEPTH);
        let mut addrs: Vec<*const CompactValue> = Vec::with_capacity(DEPTH);
        for i in 0..DEPTH {
            frames.emplace_compact_in_window(
                Arc::clone(&cached),
                &[(CompactValue::int(i as i32), b'I')],
            );
            frames[i].set_local_int_unchecked(2, -(i as i32));
            let _ = frames[i].stack.push(Value::Int(i as i32 * 3));
            addrs.push(frames[i].locals.as_ptr());
        }
        let check = |frames: &FrameStack, upto: usize| {
            for i in 0..upto {
                assert_eq!(
                    frames[i].get_local_int_unchecked(0),
                    i as i32,
                    "arg of frame {i}"
                );
                assert_eq!(
                    frames[i].get_local_int_unchecked(2),
                    -(i as i32),
                    "local of frame {i}"
                );
                assert_eq!(
                    frames[i].stack.peek_int_unchecked(),
                    i as i32 * 3,
                    "stack of frame {i}"
                );
                assert_eq!(
                    frames[i].locals.as_ptr(),
                    addrs[i],
                    "window of frame {i} moved"
                );
            }
        };
        check(&frames, DEPTH);

        // Return from the upper half the way the interpreter does, then call
        // down through the retired slots again.
        for _ in 0..DEPTH / 2 {
            assert!(frames.retire_top());
        }
        for i in DEPTH / 2..DEPTH {
            assert!(
                frames.push_cached_compact_reusing(
                    Arc::clone(&cached),
                    &[(CompactValue::int(7), b'I')],
                )
            );
            assert!(
                frames[i].locals.is_window(),
                "a retired window is rebuilt in a window"
            );
            assert_eq!(frames[i].get_local_int_unchecked(0), 7);
            assert_eq!(
                frames[i].locals.as_ptr(),
                addrs[i],
                "the same depth, the same window"
            );
            assert!(
                frames[i].stack.is_empty(),
                "a rebuilt frame starts with an empty stack"
            );
        }
        check(&frames, DEPTH / 2);
        frames.truncate(10);
        check(&frames, 10);
        frames.clear();
        frames.emplace_compact_in_window(Arc::clone(&cached), &[(CompactValue::int(1), b'I')]);
        assert_eq!(
            frames[0].locals.as_ptr(),
            addrs[0],
            "a cleared stack starts at the bottom"
        );
    }

    /// Owned and windowed frames interleave on one stack (a by-value push
    /// between two cached calls): popping past the owned frame still releases
    /// exactly the windows above the frame that stays.
    #[test]
    fn owned_frames_between_windowed_ones_release_the_right_windows() {
        let cached = cached_probe("m", "(I)V", 2, 2);
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(Arc::clone(&cached), &[(CompactValue::int(1), b'I')]);
        frames.push(test_frame("owned", 2));
        frames.emplace_compact_in_window(Arc::clone(&cached), &[(CompactValue::int(3), b'I')]);
        assert!(frames[0].locals.is_window());
        assert!(!frames[1].locals.is_window());
        assert!(frames[2].locals.is_window());
        let third = frames[2].locals.as_ptr();

        frames.truncate(1);
        frames.emplace_compact_in_window(Arc::clone(&cached), &[(CompactValue::int(4), b'I')]);
        assert_eq!(
            frames[1].locals.as_ptr(),
            third,
            "the released window is reused"
        );
        assert_eq!(
            frames[0].get_local_int_unchecked(0),
            1,
            "the frame below is intact"
        );
        assert_eq!(frames[1].get_local_int_unchecked(0), 4);

        // An owned retired slot is rebuilt in its own buffers, not a window.
        frames.truncate(0);
        frames.push(test_frame("owned", 2));
        assert!(frames.retire_top());
        assert!(frames
            .push_cached_compact_reusing(Arc::clone(&cached), &[(CompactValue::int(5), b'I')],));
        assert!(
            !frames[0].locals.is_window(),
            "pooled buffers are kept, not freed"
        );
        assert_eq!(frames[0].get_local_int_unchecked(0), 5);
    }

    /// Interpreter round i1 wave 47, lane L7 (proposal i37-L7 item 1): a
    /// retired slot rebuilt for the entry it already holds takes and drops no
    /// reference, and holds what the owned rebuild holds; a slot of another
    /// entry is rebuilt from a clone.
    #[test]
    fn a_slot_rebuilt_for_its_own_entry_takes_no_reference() {
        let cached = cached_probe("m", "(IJ)V", 4, 2);
        let args = [
            (CompactValue::int(7), b'I'),
            (CompactValue::from_value_kinded(Value::Long(1 << 40)), b'J'),
        ];
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(Arc::clone(&cached), &args);
        assert!(frames.retire_top());
        assert!(frames.retired_slot_runs(&cached));
        let count = Arc::strong_count(&cached);
        assert!(frames.push_cached_compact_reusing_same(&cached, &args));
        assert_eq!(
            Arc::strong_count(&cached),
            count,
            "no reference taken or dropped"
        );
        assert!(Arc::ptr_eq(
            frames[0].cached_method().expect("cached"),
            &cached
        ));
        let same = frame_shape(&frames[0]);

        assert!(frames.retire_top());
        assert!(frames.push_cached_compact_reusing(Arc::clone(&cached), &args));
        assert_eq!(
            frame_shape(&frames[0]),
            same,
            "the owned rebuild holds the same"
        );
        assert_eq!(Arc::strong_count(&cached), count);

        let other = cached_probe("n", "(IJ)V", 4, 2);
        assert!(frames.retire_top());
        assert!(!frames.retired_slot_runs(&other));
        assert!(frames.push_cached_compact_reusing_same(&other, &args));
        assert!(Arc::ptr_eq(
            frames[0].cached_method().expect("cached"),
            &other
        ));
        assert_eq!(Arc::strong_count(&cached), count - 1, "the slot let `m` go");
        assert_eq!(frames[0].get_local_int_unchecked(0), 7);
    }

    /// A frame that leaves by value takes owned copies: the slab reuses its
    /// window for the next push, and the copy must not change when it does.
    #[test]
    fn a_popped_windowed_frame_leaves_with_its_own_copy() {
        let cached = cached_probe("m", "(JI)V", 4, 2);
        let args = [
            (CompactValue::from_value_kinded(Value::Long(1 << 40)), b'J'),
            (CompactValue::int(5), b'I'),
        ];
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(Arc::clone(&cached), &args);
        let _ = frames[0].stack.push(Value::Int(11));
        let window = frames[0].locals.as_ptr();
        let before = frame_shape(&frames[0]);

        let popped = frames.pop().expect("one frame");
        assert!(!popped.locals.is_window() && !popped.stack.is_windowed());
        assert_eq!(frame_shape(&popped), before);
        assert_eq!(popped.stack.peek_int_unchecked(), 11);

        let other = cached_probe("n", "(II)V", 4, 2);
        frames.emplace_compact_in_window(
            other,
            &[(CompactValue::int(99), b'I'), (CompactValue::int(98), b'I')],
        );
        assert_eq!(frames[0].locals.as_ptr(), window, "the window was released");
        assert_eq!(frame_shape(&popped), before, "the popped copy is its own");

        // The `Vec<Frame>` conversions detach every live frame the same way.
        frames.push(test_frame("owned", 1));
        let v: Vec<Frame> = frames.into();
        assert_eq!(v.len(), 2);
        assert!(v
            .iter()
            .all(|f| !f.locals.is_window() && !f.stack.is_windowed()));
        assert_eq!(v[0].get_local_int_unchecked(0), 99);
        assert_eq!(v[0].get_local_int_unchecked(1), 98);
    }

    /// The collector reads a windowed frame's locals and operand stack
    /// through the same scans as a pooled frame's, and its remap writes land
    /// in the window.
    #[test]
    fn the_gc_scans_and_remaps_a_windowed_frames_locals_and_stack() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let a = heap.alloc_object(ClassId::new(0), 0);
        let b = heap.alloc_object(ClassId::new(0), 0);
        let a_moved = heap.alloc_object(ClassId::new(0), 0);
        let b_moved = heap.alloc_object(ClassId::new(0), 0);

        // gci i3/il: the probe is static, so slot 0 is live only because the
        // body reads it (`aload_0; pop; return`); with a bare `return` it is a
        // dead parameter and no root (pinned at the end of this test).
        let cached =
            cached_probe_with_code("m", "(Ljava/lang/Object;)V", 2, 2, &[0x2a, 0x57, 0xb1]);
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(
            Arc::clone(&cached),
            &[(
                CompactValue::from_value_kinded(Value::Object(Some(a))),
                b'L',
            )],
        );
        frames[0].stack.push(Value::Object(Some(b))).expect("push");
        assert!(frames[0].locals.is_window());

        let mut roots = Vec::new();
        frames[0].scan_local_objects(&mut roots, &heap);
        frames[0].stack.scan_object_refs(&mut roots, &heap);
        assert_eq!(roots, vec![a, b]);

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(a.as_ptr() as usize, a_moved.as_ptr() as usize);
        map.insert(b.as_ptr() as usize, b_moved.as_ptr() as usize);
        frames[0].update_local_refs(&map, &heap);
        frames[0].stack.update_object_refs(&map, &heap);
        assert_eq!(
            frames[0].get_local_compact(0).as_object_ptr(),
            Some(a_moved.as_ptr() as u64)
        );
        assert_eq!(
            frames[0].stack.peek_compact().as_object_ptr(),
            Some(b_moved.as_ptr() as u64)
        );

        // The static/dead twin: a static `return` body never reads its
        // parameter, so slot 0 is not a root (no receiver insurance).
        let dead = cached_probe("d", "(Ljava/lang/Object;)V", 2, 2);
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(
            dead,
            &[(
                CompactValue::from_value_kinded(Value::Object(Some(a_moved))),
                b'L',
            )],
        );
        // gci i10/np: "not a root" is the liveness answer, the `=0` arm of
        // `CRATONVM_GC_INTERP_TYPE_MAP_LOCALS`. On the default arm the dead
        // parameter is still reference-typed at entry (HotSpot's interpreter
        // type map), so it is a root there.
        let mut roots = Vec::new();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_INTERP_TYPE_MAP_LOCALS", Some("0"))],
            || frames[0].scan_local_objects(&mut roots, &heap),
        );
        assert!(roots.is_empty(), "a static method's dead slot 0 is no root");
        let mut typed_roots = Vec::new();
        cratonvm_types::flags::with_thread_overrides(&[], || {
            frames[0].scan_local_objects(&mut typed_roots, &heap)
        });
        assert_eq!(
            typed_roots,
            vec![a_moved],
            "default arm: the dead but reference-typed parameter is a root (HotSpot's interpreter oop map)"
        );
    }

    /// A retired slot rebuilt for the method it last ran keeps its code
    /// `Arc` (no increment, no decrement); rebuilt for another method it
    /// takes that method's code and releases the old one.
    #[test]
    fn a_slot_rebuilt_for_the_same_method_keeps_its_code_arc() {
        let f = cached_probe("f", "(I)I", 2, 2);
        let g = cached_probe("g", "(I)I", 2, 2);
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(Arc::clone(&f), &[(CompactValue::int(1), b'I')]);
        let held = Arc::strong_count(&f.code);
        for i in 0..4 {
            assert!(frames.retire_top());
            assert!(
                frames.push_cached_compact_reusing(Arc::clone(&f), &[(CompactValue::int(i), b'I')])
            );
            assert!(Arc::ptr_eq(&frames[0].code, &f.code));
            assert_eq!(Arc::strong_count(&f.code), held);
            assert_eq!(frames[0].get_local_int_unchecked(0), i);
        }
        assert!(frames.retire_top());
        assert!(frames.push_cached_compact_reusing(Arc::clone(&g), &[(CompactValue::int(9), b'I')]));
        assert!(Arc::ptr_eq(&frames[0].code, &g.code));
        assert_eq!(
            Arc::strong_count(&f.code),
            held - 1,
            "the old code is released"
        );
        assert_eq!(frames[0].method_name(), "g");
    }

    // ===================================================================
    // FrameStack — argument overlap (stage 2, wave 37, lane L7)
    // ===================================================================

    /// What a fast door hands the overlap: the top `tags.len()` operand-stack
    /// slots of `frame`, first argument first, read without popping
    /// (`invoke_fast::read_args_verbatim`).
    fn door_args(frame: &Frame, tags: &[u8]) -> Vec<(CompactValue, u8)> {
        let n = tags.len();
        (0..n)
            .map(|i| (frame.stack.peek_with_kind_at(n - 1 - i).0, tags[i]))
            .collect()
    }

    /// Push `vals` onto the top frame, read them as a door does, pop them,
    /// and overlap a frame for `callee` on them.
    fn call_overlapped(
        frames: &mut FrameStack,
        callee: &Arc<CachedBytecodeMethod>,
        vals: &[Value],
        tags: &[u8],
    ) -> Result<(), Arc<CachedBytecodeMethod>> {
        let top = frames.len() - 1;
        for v in vals {
            frames[top].stack.push(*v).expect("push");
        }
        let args = door_args(&frames[top], tags);
        frames[top].stack.discard_top(args.len());
        frames.push_cached_compact_overlapping(Arc::clone(callee), &args)
    }

    /// The claim stage 2 rests on: an overlapped frame holds exactly what a
    /// fresh window holds for the same call, with and without category-2
    /// arguments (one operand-stack slot, two local slots), and its locals
    /// start on the caller's first argument slot.
    #[test]
    fn an_overlapped_frame_holds_what_a_fresh_window_holds() {
        let cases: [(&str, Vec<Value>, &[u8]); 2] = [
            (
                "(ILjava/lang/Object;F)V",
                vec![Value::Int(7), Value::Object(None), Value::Float(1.5)],
                &b"ILF"[..],
            ),
            (
                "(JIDLjava/lang/Object;)V",
                vec![
                    Value::Long(-9_000_000_000),
                    Value::Int(3),
                    Value::Double(2.5),
                    Value::Object(None),
                ],
                &b"JIDL"[..],
            ),
        ];
        let slot = std::mem::size_of::<CompactValue>();
        for (desc, vals, tags) in cases {
            let caller = cached_probe("c", "()V", 2, 6);
            let callee = cached_probe("m", desc, 8, 3);
            let mut frames = FrameStack::new();
            frames.emplace_compact_in_window(Arc::clone(&caller), &[]);
            frames[0].stack.push(Value::Int(99)).expect("push");
            let (live_top, _) = frames[0].stack.window_top_and_end().expect("windowed");
            for v in &vals {
                frames[0].stack.push(*v).expect("push");
            }
            let args = door_args(&frames[0], tags);
            frames[0].stack.discard_top(args.len());
            assert!(frames
                .push_cached_compact_overlapping(Arc::clone(&callee), &args)
                .is_ok());
            assert_eq!(frames.len(), 2, "{desc}");
            assert!(frames[1].locals_overlap_caller(), "{desc}");
            assert_eq!(
                frames[1].locals.as_ptr() as usize,
                live_top,
                "{desc}: the callee's locals start on the first argument"
            );
            let mut fresh = FrameStack::new();
            fresh.emplace_compact_in_window(Arc::clone(&callee), &args);
            assert_eq!(frame_shape(&frames[1]), frame_shape(&fresh[0]), "{desc}");
            let (callee_stack, _) = frames[1].stack.window_top_and_end().expect("windowed");
            assert_eq!(
                callee_stack,
                live_top + frames[1].locals_len() * slot,
                "{desc}: the operand stack follows the locals"
            );
            assert_eq!(frames[0].stack.len(), 1, "{desc}: the caller looks popped");
            assert_eq!(frames[0].stack.peek_int_unchecked(), 99, "{desc}");
        }
    }

    /// Stage 2b (interpreter round i1 wave 38, lane L7): an overlap laid IN
    /// PLACE, from the caller's stack with no copy of the arguments, holds
    /// exactly what a fresh window holds -- including arguments that must move
    /// up past a `long` / `double` -- and a refusal leaves the arguments on the
    /// caller's stack.
    #[test]
    fn an_in_place_overlap_lays_what_a_fresh_window_holds() {
        let cases: [(&str, Vec<Value>, &[u8]); 4] = [
            (
                "(ILjava/lang/Object;F)V",
                vec![Value::Int(7), Value::Object(None), Value::Float(1.5)],
                &b"ILF"[..],
            ),
            (
                "(JIDLjava/lang/Object;)V",
                vec![
                    Value::Long(-9_000_000_000),
                    Value::Int(3),
                    Value::Double(2.5),
                    Value::Object(None),
                ],
                &b"JIDL"[..],
            ),
            (
                "(DJ)V",
                vec![Value::Double(-0.25), Value::Long(1 << 40)],
                &b"DJ"[..],
            ),
            ("()V", vec![], &b""[..]),
        ];
        for (desc, vals, tags) in cases {
            let caller = cached_probe("c", "()V", 2, 6);
            let callee = cached_probe("m", desc, 8, 3);
            let mut frames = FrameStack::new();
            frames.emplace_compact_in_window(Arc::clone(&caller), &[]);
            frames[0].stack.push(Value::Int(99)).expect("push");
            let (live_top, _) = frames[0].stack.window_top_and_end().expect("windowed");
            for v in &vals {
                frames[0].stack.push(*v).expect("push");
            }
            let args = door_args(&frames[0], tags);
            assert!(frames
                .push_cached_compact_in_place(Arc::clone(&callee), tags)
                .is_ok());
            assert_eq!(frames.len(), 2, "{desc}");
            assert!(frames[1].locals_overlap_caller(), "{desc}");
            assert_eq!(
                frames[1].locals.as_ptr() as usize,
                live_top,
                "{desc}: the callee's locals start on the first argument"
            );
            let mut fresh = FrameStack::new();
            fresh.emplace_compact_in_window(Arc::clone(&callee), &args);
            assert_eq!(frame_shape(&frames[1]), frame_shape(&fresh[0]), "{desc}");
            assert_eq!(frames[0].stack.len(), 1, "{desc}: the caller looks popped");
            assert_eq!(frames[0].stack.peek_int_unchecked(), 99, "{desc}");
        }
        // Refused (an owned caller): nothing moves, the arguments stay.
        let callee = cached_probe("m", "(I)V", 1, 1);
        let mut frames = FrameStack::new();
        frames.push(test_frame("owned", 2));
        frames[0].stack.push(Value::Int(5)).expect("push");
        assert!(frames
            .push_cached_compact_in_place(Arc::clone(&callee), b"I")
            .is_err());
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].stack.len(),
            1,
            "the argument is still the caller's"
        );
        assert_eq!(frames[0].stack.peek_int_unchecked(), 5);
    }

    /// While the callee runs, each argument is ONE root (the callee's local;
    /// the caller's stack looks popped). A value return releases the
    /// callee's slots before its push into the caller lands on them, so the
    /// returned reference is one root too; without the release it would be
    /// the caller's and the callee's, and a moving collection would remap it
    /// twice. The slot is then overlapped again at the same depth.
    #[test]
    fn an_overlapped_argument_is_one_root_and_the_value_return_releases_it() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let a = heap.alloc_object(ClassId::new(0), 0);
        let b = heap.alloc_object(ClassId::new(0), 0);
        let roots = |frames: &FrameStack| {
            let mut r = Vec::new();
            for f in frames.iter() {
                f.scan_local_objects(&mut r, &heap);
                f.stack.scan_object_refs(&mut r, &heap);
            }
            r
        };
        let caller = cached_probe("c", "()V", 1, 4);
        // gci i3/il: `aload_0; areturn` -- the static callee reads its
        // argument, so slot 0 is live at entry (no receiver insurance).
        let callee = cached_probe_with_code(
            "m",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            1,
            2,
            &[0x2a, 0xb0],
        );
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(Arc::clone(&caller), &[]);
        assert!(call_overlapped(&mut frames, &callee, &[Value::Object(Some(a))], b"L").is_ok());
        assert_eq!(
            roots(&frames),
            vec![a],
            "the argument is the callee's local only"
        );

        // The value-return arms: release, push into the caller, retire.
        frames[1].release_caller_overlap();
        assert_eq!(frames[1].locals_len(), 0);
        frames[0].stack.push(Value::Object(Some(b))).expect("push");
        assert_eq!(
            roots(&frames),
            vec![b],
            "the result is the caller's root only"
        );
        assert!(frames.retire_top());
        assert_eq!(
            frames[0].stack.peek_compact().as_object_ptr(),
            Some(b.as_ptr() as u64)
        );

        // The retired slot at depth 1 is overlapped again, on the new top.
        assert!(call_overlapped(&mut frames, &callee, &[Value::Object(Some(a))], b"L").is_ok());
        assert_eq!(frames.len(), 2);
        assert!(frames[1].locals_overlap_caller());
        assert_eq!(roots(&frames), vec![b, a]);
    }

    /// Calls nested two deep, each overlapped on its caller's live stack top:
    /// every frame keeps its values, the returns restore the slab to the
    /// caller's mark, and a plain window taken afterwards lands past the
    /// caller's whole window, not on its free stack slots.
    #[test]
    fn nested_overlaps_restore_the_slab_and_a_plain_window_lands_past_the_caller() {
        let caller = cached_probe("c", "()V", 1, 4);
        let mid = cached_probe("m", "(II)I", 3, 4);
        let leaf = cached_probe("l", "(I)I", 1, 2);
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(Arc::clone(&caller), &[]);
        let (_, caller_end) = frames[0].stack.window_top_and_end().expect("windowed");
        let mark0 = frames.slab.mark();

        frames[0].stack.push(Value::Int(5)).expect("push");
        assert!(call_overlapped(&mut frames, &mid, &[Value::Int(1), Value::Int(2)], b"II").is_ok());
        frames[1].set_local_int_unchecked(2, 77);
        frames[1].stack.push(Value::Int(10)).expect("push");
        assert!(call_overlapped(&mut frames, &leaf, &[Value::Int(3)], b"I").is_ok());
        assert_eq!(frames[2].get_local_int_unchecked(0), 3);
        assert_eq!(frames[1].get_local_int_unchecked(0), 1);
        assert_eq!(frames[1].get_local_int_unchecked(1), 2);
        assert_eq!(frames[1].get_local_int_unchecked(2), 77);
        assert_eq!(frames[1].stack.peek_int_unchecked(), 10);
        assert_eq!(frames[0].stack.peek_int_unchecked(), 5);

        // Return from the leaf, then from the middle frame, as the arms do.
        frames[2].release_caller_overlap();
        frames[1].stack.push(Value::Int(4)).expect("push");
        assert!(frames.retire_top());
        assert_eq!(frames[1].stack.len(), 2);
        assert_eq!(frames[1].stack.peek_int_unchecked(), 4);
        assert_eq!(frames[1].get_local_int_unchecked(2), 77);
        frames[1].release_caller_overlap();
        frames[0].stack.push(Value::Int(6)).expect("push");
        assert!(frames.retire_top());
        assert_eq!(frames.slab.mark(), mark0, "the caller's mark is back");

        frames.emplace_compact_in_window(Arc::clone(&leaf), &[(CompactValue::int(8), b'I')]);
        assert_eq!(frames[1].locals.as_ptr() as usize, caller_end);
        assert!(!frames[1].locals_overlap_caller());
        assert_eq!(frames[0].stack.len(), 2);
        assert_eq!(frames[0].stack.peek_int_unchecked(), 6);
    }

    /// Refusals change nothing: no caller, and an owned caller.
    #[test]
    fn an_overlap_without_a_windowed_caller_is_refused_and_changes_nothing() {
        let callee = cached_probe("m", "(I)V", 1, 2);
        let mut empty = FrameStack::new();
        assert!(empty
            .push_cached_compact_overlapping(Arc::clone(&callee), &[])
            .is_err());
        assert_eq!(empty.len(), 0);

        let mut frames = FrameStack::new();
        frames.push(test_frame("owned", 2));
        let before = frames.slab.mark();
        assert!(call_overlapped(&mut frames, &callee, &[Value::Int(1)], b"I").is_err());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames.slab.mark(), before);
    }

    // ===================================================================
    // Per-OS-thread SoA pool
    // ===================================================================

    /// A recycled buffer pair must be picked up by the non-pooled constructors
    /// and must produce a frame indistinguishable from a freshly allocated one
    /// — in particular with no stale locals and no stale kind marks.
    #[test]
    fn tls_soa_pool_is_reused_and_leaves_no_stale_state() {
        tls_soa_pool_clear();
        assert_eq!(tls_soa_pool_depths(), (0, 0));

        // Dirty a frame, then offer its buffers to the pool.
        let mut dirty = test_frame("dirty", 4);
        dirty.set_local(0, Value::Long(-1));
        dirty.set_local(2, Value::Double(2.5));
        dirty.stack.push(Value::Long(i64::MIN)).expect("push");
        let (lv, lt, sv, st) = dirty.take_pool_parts();
        offer_frame_parts_to_tls_pool(lv, lt, sv, st);
        assert_eq!(
            tls_soa_pool_depths(),
            (1, 1),
            "recycled buffers were not pooled"
        );

        // The next non-pooled construction must drain the pool …
        let reused = test_frame("reused", 4);
        assert_eq!(
            tls_soa_pool_depths(),
            (0, 0),
            "pooled buffers were not reused by Frame::new"
        );

        // … and must look exactly like a fresh frame.
        assert_eq!(reused.stack.len(), 0, "recycled operand stack not empty");
        for i in 0u16..4 {
            assert_eq!(
                reused.get_local(i),
                Value::Uninitialized,
                "recycled local {i} kept stale content"
            );
        }
        tls_soa_pool_clear();
    }

    /// A pooled frame must be byte-for-byte equivalent to an unpooled one —
    /// same locals, same argument copy-in, same operand-stack capacity.
    #[test]
    fn pooled_and_unpooled_frames_are_equivalent() {
        let args = [Value::Int(11), Value::Long(-7), Value::Object(None)];

        tls_soa_pool_clear();
        let fresh = Frame::new(
            ClassId::new(5),
            "Eq".to_string(),
            "m".to_string(),
            "(IJLjava/lang/Object;)V".to_string(),
            None,
            vec![0xb1],
            vec![],
            3,
            6,
            &args,
        );

        // Prime the pool with a dirty buffer pair, then build the same frame.
        let mut dirty = test_frame("dirty", 6);
        dirty.set_local(0, Value::Long(i64::MIN));
        dirty.set_local(1, Value::Double(-1.0));
        let (lv, lt, sv, st) = dirty.take_pool_parts();
        offer_frame_parts_to_tls_pool(lv, lt, sv, st);
        let pooled = Frame::new(
            ClassId::new(5),
            "Eq".to_string(),
            "m".to_string(),
            "(IJLjava/lang/Object;)V".to_string(),
            None,
            vec![0xb1],
            vec![],
            3,
            6,
            &args,
        );

        assert_eq!(pooled.max_locals, fresh.max_locals);
        for i in 0u16..fresh.max_locals {
            assert_eq!(
                pooled.get_local(i),
                fresh.get_local(i),
                "pooled local {i} differs from the unpooled frame"
            );
        }
        assert_eq!(pooled.stack.len(), fresh.stack.len());
        tls_soa_pool_clear();
    }

    /// Oversized buffers must not be pinned for the lifetime of the thread.
    #[test]
    fn tls_soa_pool_declines_oversized_buffers() {
        tls_soa_pool_clear();
        let huge = vec![0u64; TLS_SOA_MAX_RETAINED_SLOTS + 1];
        offer_frame_parts_to_tls_pool(huge, Vec::new(), Vec::new(), Vec::new());
        assert_eq!(
            tls_soa_pool_depths(),
            (0, 1),
            "oversized locals buffer must be dropped; the small stack half is \
             still poolable"
        );
        tls_soa_pool_clear();
    }

    /// The pool must saturate rather than grow without bound.
    #[test]
    fn tls_soa_pool_is_capped() {
        tls_soa_pool_clear();
        for _ in 0..(TLS_SOA_POOL_CAP * 2) {
            offer_frame_parts_to_tls_pool(Vec::new(), Vec::new(), Vec::new(), Vec::new());
        }
        assert_eq!(
            tls_soa_pool_depths(),
            (TLS_SOA_POOL_CAP, TLS_SOA_POOL_CAP),
            "TLS SoA pool exceeded its cap"
        );
        tls_soa_pool_clear();
    }

    // ===================================================================
    // Per-method padded-bytecode memo
    // ===================================================================

    /// zt t3/fp: at the limit, the keep-live eviction drops only the entries
    /// nobody else holds and sets the next limit to twice what it kept (never
    /// below the cap); the kill arm clears whole (the positive control: a live
    /// entry is lost).
    #[test]
    fn zt3fp_padded_code_eviction_keeps_entries_live_methods_hold() {
        let mut cache: HashMap<PaddedCodeKey, PaddedCodeEntry> = HashMap::new();
        let live = padded_bytecode(&[0xb1]);
        cache.insert((1, 1), (Arc::clone(&live), Vec::new().into_boxed_slice()));
        cache.insert((1, 2), (padded_bytecode(&[0x04, 0xac]), Vec::new().into_boxed_slice()));
        cache.insert((2, 1), (padded_bytecode(&[0x00, 0xb1]), Vec::new().into_boxed_slice()));
        let limit = evict_padded_code(&mut cache, true);
        assert_eq!(cache.len(), 1, "only the held entry stays");
        assert!(Arc::ptr_eq(&cache[&(1, 1)].0, &live));
        assert_eq!(limit, PADDED_CODE_CACHE_CAP, "never below the cap");
        // A table of live entries larger than half the cap raises the limit.
        let held: Vec<Arc<[u8]>> = (0..PADDED_CODE_CACHE_CAP as u64)
            .map(|i| {
                let body = padded_bytecode(&[0xb1]);
                cache.insert((3, i), (Arc::clone(&body), Vec::new().into_boxed_slice()));
                body
            })
            .collect();
        let limit = evict_padded_code(&mut cache, true);
        assert_eq!(cache.len(), held.len() + 1);
        assert_eq!(limit, 2 * cache.len());
        // The kill arm clears whole, live entries included.
        assert_eq!(evict_padded_code(&mut cache, false), PADDED_CODE_CACHE_CAP);
        assert!(cache.is_empty());
        drop(held);
    }

    #[test]
    fn padded_bytecode_memo_returns_the_same_arc_for_the_same_method() {
        let code = vec![0x1a, 0x04, 0x60, 0xac];
        let a = padded_bytecode_for_method(ClassId::new(4242), "memoA", "()I", &code, &[]);
        let b = padded_bytecode_for_method(ClassId::new(4242), "memoA", "()I", &code, &[]);
        assert!(Arc::ptr_eq(&a, &b), "identical method must reuse the Arc");
        assert_eq!(&a[..code.len()], &code[..]);
        assert_eq!(a.len(), code.len() + 2);
        assert_eq!(a[code.len()], 0);
        assert_eq!(a[code.len() + 1], 0);
    }

    /// Distinct methods must NEVER share an Arc even when their bodies are
    /// byte-identical — `local_liveness.rs` keys its per-method table on
    /// `Arc::as_ptr(code)` and computes it from the method's exception table,
    /// which is not part of the bytes.
    #[test]
    fn padded_bytecode_memo_never_merges_distinct_methods() {
        let code = vec![0xb1];
        let a = padded_bytecode_for_method(ClassId::new(7), "same", "()V", &code, &[]);
        let other_name =
            padded_bytecode_for_method(ClassId::new(7), "different", "()V", &code, &[]);
        let other_desc = padded_bytecode_for_method(ClassId::new(7), "same", "(I)V", &code, &[]);
        let other_class = padded_bytecode_for_method(ClassId::new(8), "same", "()V", &code, &[]);
        assert!(!Arc::ptr_eq(&a, &other_name));
        assert!(!Arc::ptr_eq(&a, &other_desc));
        assert!(!Arc::ptr_eq(&a, &other_class));
    }

    /// Redefinition rewrites the body under an unchanged identity; the memo
    /// must notice and mint a fresh Arc rather than serve the old bytes.
    #[test]
    fn padded_bytecode_memo_detects_a_redefined_body() {
        let v1 = vec![0xb1];
        let v2 = vec![0x04, 0xac];
        let a = padded_bytecode_for_method(ClassId::new(99), "redef", "()V", &v1, &[]);
        let b = padded_bytecode_for_method(ClassId::new(99), "redef", "()V", &v2, &[]);
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(&b[..v2.len()], &v2[..]);
        // The old Arc is untouched — existing frames keep executing old code.
        assert_eq!(&a[..v1.len()], &v1[..]);
    }

    /// Wave 79 (lane L7): a redefinition that rewrites only the handler
    /// ranges, or another VM's method under the same per-VM class id with the
    /// same bytes and other ranges, must not be handed the first `Arc`:
    /// `local_liveness` keys its table on the `Arc` and derives it from the
    /// exception table
    /// (`i77-L4-padded-code-cache-shares-a-body-across-vms-and-handler-only-redefinitions-20261110.md`).
    #[test]
    fn padded_bytecode_memo_keys_on_the_exception_table_too() {
        let code = vec![0x00, 0x00, 0xb1, 0x2d, 0xbf];
        let narrow = [ExceptionTableEntry {
            start_pc: 0,
            end_pc: 1,
            handler_pc: 3,
            catch_type: 0,
        }];
        let wide = [ExceptionTableEntry {
            start_pc: 0,
            end_pc: 2,
            handler_pc: 3,
            catch_type: 0,
        }];
        let cid = ClassId::new(790_007);
        let a = padded_bytecode_for_method(cid, "handlers", "()V", &code, &narrow);
        let b = padded_bytecode_for_method(cid, "handlers", "()V", &code, &wide);
        assert!(
            !Arc::ptr_eq(&a, &b),
            "a handler-only change mints a fresh Arc"
        );
        let c = padded_bytecode_for_method(cid, "handlers", "()V", &code, &wide);
        assert!(
            Arc::ptr_eq(&b, &c),
            "the same body and table still share one"
        );
        let none = padded_bytecode_for_method(cid, "handlers", "()V", &code, &[]);
        assert!(!Arc::ptr_eq(&c, &none), "no handlers is another table");
        assert_eq!(&none[..code.len()], &code[..]);
    }

    /// `Frame::new` goes through the memo, so two frames for the same method
    /// share one bytecode allocation instead of copying the body per frame.
    #[test]
    fn frame_new_shares_bytecode_across_frames_of_the_same_method() {
        let f1 = Frame::new(
            ClassId::new(31337),
            "Shared".to_string(),
            "body".to_string(),
            "()V".to_string(),
            None,
            vec![0x03, 0xac],
            vec![],
            1,
            1,
            &[],
        );
        let f2 = Frame::new(
            ClassId::new(31337),
            "Shared".to_string(),
            "body".to_string(),
            "()V".to_string(),
            None,
            vec![0x03, 0xac],
            vec![],
            1,
            1,
            &[],
        );
        assert!(
            Arc::ptr_eq(&f1.code, &f2.code),
            "per-frame bytecode copy was not eliminated"
        );
        // Padding contract still holds (the hot loop reads code[pc+1..2]).
        assert!(f1.code.len() >= 2);
        assert_eq!(f1.code[f1.code.len() - 1], 0);
        assert_eq!(f1.code[f1.code.len() - 2], 0);
    }
