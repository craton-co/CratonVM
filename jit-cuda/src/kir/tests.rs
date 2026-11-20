    use super::*;
    use std::fmt::Write;

    /// The `fmt::Write` impl is what lets 84 unconverted `writeln!`
    /// sites keep working, so the line splitting it does has to be
    /// exactly right — an off-by-one newline would move every golden.
    #[test]
    fn writeln_produces_one_instruction_per_line_including_its_newline() {
        let mut b = Body::new();
        writeln!(b, "    add.s32 %r1, %r2, %r3;").unwrap();
        writeln!(b, "    bar.sync 0;").unwrap();
        writeln!(b, "    ret;").unwrap();
        assert_eq!(b.len(), 3);
        // A line the grammar covers is lifted; one it does not stays text.
        assert!(matches!(
            b.code()[0],
            Instr::Bin {
                op: BinOp::Add,
                ty: Ty::S32,
                ..
            }
        ));
        assert_eq!(b.code()[1], Instr::Raw("    bar.sync 0;\n".to_string()));
        assert_eq!(
            b.render_ptx(),
            "    add.s32 %r1, %r2, %r3;\n    bar.sync 0;\n    ret;\n"
        );
    }

    /// `write_fmt` may deliver one line across several `write_str`
    /// calls, so a partial write must not become its own instruction.
    #[test]
    fn a_line_split_across_writes_is_still_one_instruction() {
        let mut b = Body::new();
        b.write_str("    add").unwrap();
        b.write_str(".s32 %r1, %r2, %r3;").unwrap();
        // `code()` rather than `len()`: `len()` is the splice-point
        // accessor and deliberately asserts it is not called mid-line,
        // which is exactly the state under test here.
        assert_eq!(
            b.code().len(),
            0,
            "nothing is an instruction until its newline"
        );
        b.write_str("\n").unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b.render_ptx(), "    add.s32 %r1, %r2, %r3;\n");
    }

    /// A multi-line `writeln!` (the emitter has several) splits into
    /// one instruction per line.
    #[test]
    fn a_multi_line_write_splits_per_line() {
        let mut b = Body::new();
        write!(b, "    a;\n    b;\n").unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b.render_ptx(), "    a;\n    b;\n");
    }

    /// Every structured variant renders the text its `Raw` predecessor
    /// did. These four spellings are load-bearing: they are what the
    /// goldens recorded before the variants existed.
    #[test]
    fn structured_variants_render_the_text_they_replaced() {
        let mut b = Body::new();
        b.push(Instr::ReadSpecial {
            dst: "%r3".into(),
            which: SpecialReg::ThreadIdxX,
        });
        b.push(Instr::ReadSpecial {
            dst: "%r4".into(),
            which: SpecialReg::BlockDimX,
        });
        b.push(Instr::ReadSpecial {
            dst: "%r5".into(),
            which: SpecialReg::BlockIdxX,
        });
        b.push(Instr::Label("L_done".into()));
        b.push(Instr::Bra {
            target: "L_done".into(),
            pred: None,
        });
        b.push(Instr::Bra {
            target: "L_done".into(),
            pred: Some(PredUse {
                reg: "%p1".into(),
                negated: false,
            }),
        });
        b.push(Instr::Bra {
            target: "L_done".into(),
            pred: Some(PredUse {
                reg: "%p1".into(),
                negated: true,
            }),
        });
        b.push(Instr::Ret);
        let want = concat!(
            "    mov.u32 %r3, %tid.x;
",
            "    mov.u32 %r4, %ntid.x;
",
            "    mov.u32 %r5, %ctaid.x;
",
            "L_done:
",
            "    bra L_done;
",
            "    @%p1 bra L_done;
",
            "    @!%p1 bra L_done;
",
            "    ret;
",
        );
        assert_eq!(b.render_ptx(), want);
    }

    /// `has_branch_since` replaces a `contains("bra ")` over a byte
    /// range, and has to see a branch in either representation — during
    /// the migration a body holds both.
    #[test]
    fn a_branch_is_seen_whether_it_is_structured_or_raw() {
        let mut b = Body::new();
        writeln!(b, "    add.s32 %r1, %r2, %r3;").unwrap();
        let mark = b.len();
        assert!(!b.has_branch_since(mark));
        b.push(Instr::Bra {
            target: "L".into(),
            pred: None,
        });
        assert!(b.has_branch_since(mark));

        let mut r = Body::new();
        writeln!(r, "    add.s32 %r1, %r2, %r3;").unwrap();
        let mark = r.len();
        writeln!(r, "    bra L_done;").unwrap();
        assert!(r.has_branch_since(mark));
    }

    /// The splice is what the bounds prologue needs, and it has to land
    /// the instructions in order at the recorded point.
    #[test]
    fn a_splice_lands_in_order_at_the_recorded_index() {
        let mut b = Body::new();
        writeln!(b, "    guard;").unwrap();
        let at = b.len();
        writeln!(b, "    body;").unwrap();
        let mut pro = Body::new();
        writeln!(pro, "    pre1;").unwrap();
        writeln!(pro, "    pre2;").unwrap();
        b.splice_at(at, pro);
        assert_eq!(
            b.render_ptx(),
            "    guard;\n    pre1;\n    pre2;\n    body;\n"
        );
    }
