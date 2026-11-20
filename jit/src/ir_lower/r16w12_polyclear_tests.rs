    use super::*;
    use crate::ir::IrBuilder;
    use crate::ir_schedule;
    use cratonvm_types::flags::with_thread_overrides;


    fn no_helpers() -> JitRuntimeHelpers {
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the bodies here call no helper.
        unsafe { std::mem::zeroed() }
    }


    /// The fixture (`a + 1`: no loop, no call, no splice, no snapshot) holds
    /// a register a deopt could name, and no reachable snapshot names it: the
    /// whole image goes, and only the image and the staging below it move --
    /// exactly the layout `size_deopt_image` gives an empty plan.
    #[test]
    fn r16w12_an_exitless_body_reserves_no_image() {
        let _ls = LsForce::on();
        let _dr = DeoptRegsForce::on();
        for &reg in IR_LOWER_LS_GPRS {
            for sized in [false, true] {
                let mut lo = super::tests::lowerer_with_resident_gpr(16384, reg);
                let (frame, stage_top, spill_cap) =
                    (lo.frame_size, lo.args_stage_top_off, lo.spill_cap_off);
                lo.size_deopt_image(sized);
                assert!(
                    lo.deopt_regs_base > 0,
                    "r{reg} sized={sized}: an image to drop"
                );
                assert!(
                    lo.deopt_image_never_read(),
                    "r{reg}: the fixture has no exit"
                );
                assert!(
                    !lo.drop_unread_deopt_image(false),
                    "r{reg} sized={sized}: off drops nothing"
                );
                assert!(lo.deopt_regs_base > 0);
                assert!(lo.drop_unread_deopt_image(true), "r{reg} sized={sized}");
                assert_eq!(lo.deopt_regs_base, 0, "r{reg} sized={sized}");
                assert_eq!(lo.deopt_regs_bytes, 0, "r{reg} sized={sized}");
                assert_eq!(
                    lo.frame_size,
                    frame - DEOPT_IMAGE_FULL_BYTES,
                    "r{reg} sized={sized}: the frame loses the whole image"
                );
                assert_eq!(lo.frame_size % 16, frame % 16, "r{reg}: alignment kept");
                assert_eq!(
                    lo.args_stage_top_off,
                    stage_top - DEOPT_IMAGE_FULL_BYTES,
                    "r{reg} sized={sized}"
                );
                assert_eq!(
                    lo.spill_cap_off, spill_cap,
                    "r{reg}: shallower offsets stay"
                );
                assert_eq!(lo.deopt_gpr_image(), (0, 0), "r{reg}: nothing published");
                assert_eq!(lo.deopt_xmm_image(), None);
                assert!(!lo.deopt_image_holds_gpr(reg));
                assert_eq!(
                    lo.register_frame_value(0, IrType::Long),
                    None,
                    "r{reg}: a frame state can no longer name the register"
                );
                // Idempotent.
                let after = (lo.frame_size, lo.args_stage_top_off);
                assert!(!lo.drop_unread_deopt_image(true));
                assert_eq!((lo.frame_size, lo.args_stage_top_off), after);
            }
        }
    }

    /// The predicate: a primitive value a deopt could name by its register
    /// AND a reachable snapshot names AND whose home was dropped keeps the
    /// image; any of the three alone, a kept home (the snapshot then names
    /// the home word), or a non-primitive value, does not. A spliced body
    /// keeps it whatever the tables say.
    #[test]
    fn r16w12_a_reachable_register_naming_keeps_the_image() {
        let _ls = LsForce::on();
        let _dr = DeoptRegsForce::on();
        let reg = *IR_LOWER_LS_GPRS.first().expect("a GP residency file");
        // `add_one_graph`: node 3 is `Param(0)`, an `int`; node 0 is `Start`.
        let param = 3usize;
        let fresh = || {
            let lo = super::tests::lowerer_with_resident_gpr(16384, reg);
            assert_eq!(lo.graph.nodes[param].op, Op::Param(0), "fixture layout");
            assert!(lo.deopt_named_reachable.len() > param);
            assert!(lo.deopt_nameable.len() > param);
            lo
        };
        let mut all = fresh();
        assert!(all.home_dropped.len() > param);
        all.deopt_nameable[param] = true;
        all.deopt_named_reachable[param] = true;
        all.home_dropped[param] = true;
        assert!(
            !all.deopt_image_never_read(),
            "nameable, reachable, no home"
        );
        let frame = all.frame_size;
        assert!(!all.drop_unread_deopt_image(true));
        assert_eq!(all.frame_size, frame, "the image stays");
        assert_eq!(all.deopt_regs_bytes, DEOPT_IMAGE_FULL_BYTES);

        let mut kept_home = fresh();
        kept_home.deopt_nameable[param] = true;
        kept_home.deopt_named_reachable[param] = true;
        kept_home.home_dropped[param] = false;
        assert!(
            kept_home.deopt_image_never_read(),
            "a kept home describes the value"
        );

        let mut nameable_only = fresh();
        nameable_only.deopt_nameable[param] = true;
        nameable_only.deopt_named_reachable[param] = false;
        nameable_only.home_dropped[param] = true;
        assert!(
            nameable_only.deopt_image_never_read(),
            "no reachable naming"
        );

        let mut reachable_only = fresh();
        reachable_only.deopt_nameable[param] = false;
        reachable_only.deopt_named_reachable[param] = true;
        reachable_only.home_dropped[param] = true;
        assert!(
            reachable_only.deopt_image_never_read(),
            "no register to name"
        );

        // Node 0 (`Start`, the fixture's register holder) is not a primitive.
        let mut control = fresh();
        assert!(
            control.deopt_nameable[0],
            "fixture: node 0 owns its register"
        );
        control.deopt_named_reachable[0] = true;
        control.home_dropped[0] = true;
        assert!(
            control.deopt_image_never_read(),
            "only Int/Long are named by register"
        );

        let mut spliced = fresh();
        spliced.spliced_ranges = &[(0, 1, 2)];
        assert!(
            !spliced.deopt_image_never_read(),
            "a spliced body keeps its image"
        );
    }

    /// Executed: a deopt through a frame whose image was dropped hands
    /// `ir_deopt_entry` NULL and still reconstructs a frame word, with the
    /// residency register saved and restored around it.
    #[test]
    fn r16w12_a_deopt_through_an_exitless_frame_reads_its_frame_words() {
        let _ls = LsForce::on();
        let _dr = DeoptRegsForce::on();
        let mut lo = super::tests::lowerer_with_resident_gpr(16384, 3);
        assert!(lo.drop_unread_deopt_image(true));
        assert_eq!(lo.deopt_regs_base, 0);
        let locals = super::r16w11_frames2_tests::deopt_through(lo, None);
        assert_eq!(
            locals,
            vec![FrameValue::Long(super::r16w11_frames2_tests::POISON)]
        );
    }

    /// The fresh-frame clear's `_SKIP_UNROOTED` has nothing left to leave
    /// out: every word of the smaller frame is cleared.
/// `static int f(int a, int b) { return a * b + 7; }` -- a trap-free,
    /// loop-free, call-free leaf like `R16Bench`'s `Shape.area()` bodies.
    const LEAF: [u8; 9] = [
        0x1a, 0x1b, 0x68, // iload_0; iload_1; imul
        0x10, 0x07, 0x60, // bipush 7; iadd
        0xac, // ireturn
        0x00, 0x00,
    ];

    fn lower_leaf(edits: &[(&str, Option<&str>)]) -> CompiledMethod {
        with_thread_overrides(edits, || {
            let mut builder = IrBuilder::new(2, 2);
            builder.set_param_types(&[IrType::Int, IrType::Int]);
            let graph = builder.build(&LEAF, 7).expect("IR build");
            assert!(graph_cannot_deopt(&graph), "fixture: a trap-free leaf");
            let schedule = ir_schedule::schedule(&graph);
            lower(&graph, &schedule, 2, 2, &no_helpers()).expect("the leaf lowers")
        })
    }


    /// Round 16 wave 13: a register-nameable value that is also a monitor
    /// object keeps the image even with its home kept -- its home word would
    /// be described `int` in the locals and `ref` as the monitor
    /// (`SlotTypeConflict`). Both ways of being a monitor object count.
    #[test]
    fn r16w13_a_monitor_object_keeps_the_image() {
        let _ls = LsForce::on();
        let _dr = DeoptRegsForce::on();
        let reg = *IR_LOWER_LS_GPRS.first().expect("a GP residency file");
        let param = 3usize;
        let marked = || {
            let mut lo = super::tests::lowerer_with_resident_gpr(16384, reg);
            assert_eq!(lo.graph.nodes[param].op, Op::Param(0), "fixture layout");
            lo.deopt_nameable[param] = true;
            lo.deopt_named_reachable[param] = true;
            lo.home_dropped[param] = false;
            assert!(lo.deopt_image_never_read(), "a kept home alone drops it");
            lo
        };
        let mut operand = marked();
        operand.live_monitor_operands.insert(3);
        assert!(
            !operand.deopt_image_never_read(),
            "a live monitor operand keeps the image"
        );
        let frame = operand.frame_size;
        assert!(!operand.drop_unread_deopt_image(true));
        assert_eq!(operand.frame_size, frame);
    }

