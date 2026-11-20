    use super::*;

    /// `iload_1; ifeq 11; aload_0; invokevirtual #2; goto 4; return`: the
    /// call at pc 5 is in a loop.
    const LOOPING: [u8; 12] = [
        0x1b, 0x99, 0x00, 0x0a, 0x2a, 0xb6, 0x00, 0x02, 0xa7, 0xff, 0xfc, 0xb1,
    ];
    /// `iload_1; ifeq 8; aload_0; invokevirtual #2; iconst_1; ireturn`:
    /// the call at pc 5 is in no loop.
    const BEHIND: [u8; 10] = [0x1b, 0x99, 0x00, 0x07, 0x2a, 0xb6, 0x00, 0x02, 0x04, 0xac];
    /// `aload_0; invokevirtual #2; ireturn`: a straight body, call at 1.
    const STRAIGHT: [u8; 5] = [0x2a, 0xb6, 0x00, 0x02, 0xac];

    fn body(code: &[u8]) -> InlineSite {
        InlineSite {
            callee_code: code.to_vec(),
            callee_code_len: code.len(),
            ..InlineSite::default()
        }
    }

    fn row(callee_pc: usize, site: InlineSite) -> NestedInlineSite {
        NestedInlineSite {
            callee_pc,
            guard_class_id: 0,
            site,
            bimorphic_second: None,
        }
    }

    /// A loop-rooted top body (`LOOPING`) whose in-loop call (pc 5) is a
    /// straight body that itself splices a leaf (pc 1): `Start.match`
    /// -> `Greedy` -> `LastNode` in shape.
    fn loop_rooted() -> InlineSite {
        let mut mid = body(&STRAIGHT);
        mid.nested_sites.push(row(1, body(&[0xac])));
        let mut top = body(&LOOPING);
        top.nested_sites.push(row(5, mid));
        top
    }

    #[test]
    fn rx27_rows_under_a_loop_are_the_in_loop_row_and_everything_below_it() {
        assert_eq!(in_loop_rows(&loop_rooted()), 2);
        // The same tree behind a branch in no loop: nothing is in a loop.
        let mut straight = loop_rooted();
        straight.callee_code = BEHIND.to_vec();
        straight.callee_code_len = BEHIND.len();
        assert_eq!(in_loop_rows(&straight), 0);
        // A loop deeper down counts from there: a straight top body whose
        // row (pc 1) is a looping body with an in-loop row (pc 5).
        let mut inner = body(&LOOPING);
        inner.nested_sites.push(row(5, body(&[0xac])));
        let mut top = body(&STRAIGHT);
        top.nested_sites.push(row(1, inner));
        assert_eq!(in_loop_rows(&top), 1);
    }

    /// The site table `append_ir_inline_site` writes for `loop_rooted`
    /// spliced at caller pc 2: the top body at 8, its pc-5 row at 20,
    /// that row's pc-1 leaf at 28.
    fn table(with_leaf: bool) -> (HashMap<usize, ir::IrInlineSite>, Vec<u8>) {
        let at = |base: usize, code_len: usize| ir::IrInlineSite {
            base,
            code_len,
            num_args: 1,
            max_locals: 2,
            arg_local_slots: vec![0],
            returns_value: true,
            receiver_is_arg0: true,
            method_key: String::new(),
            class_id: 0,
        };
        let mut combined = vec![0u8; 8];
        combined.extend_from_slice(&LOOPING);
        combined.resize(20, 0);
        combined.extend_from_slice(&STRAIGHT);
        combined.resize(28, 0);
        combined.push(0xac);
        let mut sites = HashMap::new();
        sites.insert(2, at(8, LOOPING.len()));
        sites.insert(8 + 5, at(20, STRAIGHT.len()));
        if with_leaf {
            sites.insert(20 + 1, at(28, 1));
        }
        // A bimorphic second body's tagged key sits in no body's range.
        sites.insert(ir::bimorphic_second_site_key(8 + 5), at(28, 1));
        (sites, combined)
    }

    #[test]
    fn rx27_the_spliced_count_reads_back_what_the_append_wrote() {
        let (sites, combined) = table(true);
        assert_eq!(spliced_in_loop_rows(&sites, &combined, 2), 2);
        // The leaf fell back to its call (the budget ran out): one less.
        let (sites, combined) = table(false);
        assert_eq!(spliced_in_loop_rows(&sites, &combined, 2), 1);
        // No top-level row: nothing spliced.
        assert_eq!(spliced_in_loop_rows(&sites, &combined, 3), 0);
    }

    #[test]
    fn rx27_a_loop_root_that_lost_an_in_loop_row_keeps_its_call() {
        // The append held every planned in-loop row: spliced.
        assert!(!keeps_call(2, 2, 2, false));
        // The budget ran out inside the loop: kept as a call.
        assert!(keeps_call(2, 2, 1, false));
        // A nested strip dropped one: a loss only under the strips switch.
        assert!(!keeps_call(3, 2, 2, false));
        assert!(keeps_call(3, 2, 2, true));
        // The rule reads the tree end to end on the fixture: the planned
        // tree holds 2 in-loop rows; the append wrote 1.
        let planned = in_loop_rows(&loop_rooted());
        let (sites, combined) = table(false);
        let spliced = spliced_in_loop_rows(&sites, &combined, 2);
        assert!(keeps_call(planned, planned, spliced, false));
        let line = census_line("P.s()V", 2, "R.r()V", planned, planned, spliced, 181, true);
        assert!(line.starts_with("[cratonvm-jitc] ir-loop-root-own-compile P.s()V at pc=2"));
        assert!(
            line.contains("resolved=2 planned=2 spliced=1 budget_left=181 -> kept as a call")
        );
    }


