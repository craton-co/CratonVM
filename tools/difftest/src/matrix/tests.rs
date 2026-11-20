use super::*;

/// Build a minimal but *real* class file: a constant pool with the one
/// `Utf8` the scanner needs (`"Code"`), no fields, and one method whose
/// `Code` attribute carries `body`.
///
/// Hand-built rather than compiled so the matrix generator's own test does
/// not need `javac` on PATH — and so a "known corpus" is known exactly.
fn class_with_code(body: &[u8], handlers: u16) -> Vec<u8> {
    let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x41];
    // constant_pool_count = 2 ⇒ one entry, #1 = Utf8 "Code".
    b.extend_from_slice(&2u16.to_be_bytes());
    b.push(1);
    b.extend_from_slice(&4u16.to_be_bytes());
    b.extend_from_slice(b"Code");
    // access_flags, this_class, super_class, interfaces_count = 0
    b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
    // fields_count = 0
    b.extend_from_slice(&0u16.to_be_bytes());
    // methods_count = 1
    b.extend_from_slice(&1u16.to_be_bytes());
    // access_flags, name_index, descriptor_index
    b.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    // attributes_count = 1, name_index = #1 ("Code")
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());

    // Code payload.
    let mut code_attr: Vec<u8> = Vec::new();
    code_attr.extend_from_slice(&1u16.to_be_bytes()); // max_stack
    code_attr.extend_from_slice(&1u16.to_be_bytes()); // max_locals
    code_attr.extend_from_slice(&(body.len() as u32).to_be_bytes());
    code_attr.extend_from_slice(body);
    code_attr.extend_from_slice(&handlers.to_be_bytes());
    for _ in 0..handlers {
        code_attr.extend_from_slice(&[0; 8]);
    }
    code_attr.extend_from_slice(&0u16.to_be_bytes()); // attributes_count

    b.extend_from_slice(&(code_attr.len() as u32).to_be_bytes());
    b.extend_from_slice(&code_attr);
    b
}

#[test]
fn opcode_names_cover_the_whole_jvms_range() {
    assert_eq!(opcode_name(0x00), Some("nop"));
    assert_eq!(opcode_name(0x60), Some("iadd"));
    assert_eq!(opcode_name(0xb6), Some("invokevirtual"));
    assert_eq!(opcode_name(0xba), Some("invokedynamic"));
    assert_eq!(opcode_name(0xc9), Some("jsr_w"));
    // 0xca (breakpoint) and up are reserved and must not be named.
    assert_eq!(opcode_name(0xca), None);
    assert_eq!(opcode_name(0xff), None);
    // Every named opcode must have a decodable length (except the three
    // variable-length ones, which `instruction_len` handles specially).
    for op in 0u8..=0xc9 {
        if matches!(op, 0xaa | 0xab | 0xc4) {
            continue;
        }
        assert!(fixed_len(op).is_some(), "no length for opcode {op:#04x}");
    }
}

#[test]
fn instruction_len_handles_the_variable_length_opcodes() {
    // tableswitch at pc 0: pad = 3, default+low+high = 12, 2 offsets = 8.
    let mut code = vec![0xaa, 0, 0, 0];
    code.extend_from_slice(&0i32.to_be_bytes()); // default
    code.extend_from_slice(&1i32.to_be_bytes()); // low
    code.extend_from_slice(&2i32.to_be_bytes()); // high
    code.extend_from_slice(&0i32.to_be_bytes());
    code.extend_from_slice(&0i32.to_be_bytes());
    assert_eq!(instruction_len(&code, 0), Some(24));

    // lookupswitch at pc 0: pad = 3, default + npairs = 8, 1 pair = 8.
    let mut look = vec![0xab, 0, 0, 0];
    look.extend_from_slice(&0i32.to_be_bytes());
    look.extend_from_slice(&1i32.to_be_bytes());
    look.extend_from_slice(&[0; 8]);
    assert_eq!(instruction_len(&look, 0), Some(20));

    // wide iload = 4 bytes, wide iinc = 6.
    assert_eq!(instruction_len(&[0xc4, 0x15, 0, 1], 0), Some(4));
    assert_eq!(instruction_len(&[0xc4, 0x84, 0, 1, 0, 1], 0), Some(6));
    // A reserved opcode has no length rather than a guessed one.
    assert_eq!(instruction_len(&[0xca], 0), None);
}

#[test]
fn operand_forms_are_bounded_and_specific() {
    assert_eq!(operand_form(&[0x60], 0), "implicit"); // iadd
    assert_eq!(operand_form(&[0x10, 5], 0), "s8"); // bipush
    assert_eq!(operand_form(&[0x12, 3], 0), "cp8"); // ldc
    assert_eq!(operand_form(&[0x15, 2], 0), "local:u8"); // iload
    assert_eq!(operand_form(&[0xa7, 0xff, 0xf0], 0), "branch:s16"); // goto
    assert_eq!(operand_form(&[0xbc, 10], 0), "atype:int"); // newarray int
    assert_eq!(operand_form(&[0xbc, 7], 0), "atype:double");
    assert_eq!(operand_form(&[0xc5, 0, 3, 2], 0), "cp16,dims:2");
    assert_eq!(operand_form(&[0xc4, 0x84, 0, 1, 0, 1], 0), "wide:iinc");
    assert_eq!(operand_form(&[0xb9, 0, 3, 1, 0], 0), "cp16,count");
}

#[test]
fn scans_a_known_class_and_reports_it_correctly() {
    // `iload_0; bipush 7; iadd; ireturn` — four opcodes, three forms.
    let class = class_with_code(&[0x1a, 0x10, 0x07, 0x60, 0xac], 0);
    let scan = scan_class_bytes("Known", &class).expect("well-formed class");
    assert_eq!(scan.program, "Known");
    assert_eq!(scan.methods, 1);
    assert_eq!(scan.sites.len(), 4);
    assert!(scan.sites.contains_key("26:implicit")); // iload_0
    assert!(scan.sites.contains_key("16:s8")); // bipush
    assert!(scan.sites.contains_key("96:implicit")); // iadd
    assert!(scan.sites.contains_key("172:implicit")); // ireturn
                                                      // No handlers, no back-edge.
    for facts in scan.sites.values() {
        assert!(!facts.in_exception_method);
        assert!(!facts.in_loop_method);
    }
}

#[test]
fn a_backward_branch_marks_the_method_osr_eligible() {
    // `iload_0; ifne -2; return` — the `ifne` jumps backwards.
    let class = class_with_code(&[0x1a, 0x9a, 0xff, 0xfe, 0xb1], 0);
    let scan = scan_class_bytes("Loop", &class).expect("well-formed");
    assert!(scan.sites.values().all(|f| f.in_loop_method));
    // A forward branch does not.
    let fwd = class_with_code(&[0x1a, 0x9a, 0x00, 0x03, 0xb1], 0);
    let scan = scan_class_bytes("Fwd", &fwd).expect("well-formed");
    assert!(scan.sites.values().all(|f| !f.in_loop_method));
}

#[test]
fn a_handler_table_and_athrow_both_reach_the_exception_axis() {
    let with_handler = class_with_code(&[0x01, 0xb1], 1);
    let scan = scan_class_bytes("Handled", &with_handler).expect("well-formed");
    assert!(scan.sites.values().all(|f| f.in_exception_method));

    // No handler table, but `athrow` is itself how an exception is raised.
    let throws = class_with_code(&[0x01, 0xbf], 0);
    let scan = scan_class_bytes("Throws", &throws).expect("well-formed");
    assert!(scan.sites["191:implicit"].in_exception_method, "athrow");
    assert!(!scan.sites["1:implicit"].in_exception_method, "aconst_null");
}

#[test]
fn a_malformed_class_is_reported_not_guessed_at() {
    assert!(scan_class_bytes("X", b"not a class").is_none());
    assert!(scan_class_bytes("X", &[0xCA, 0xFE, 0xBA, 0xBE]).is_none());
}

/// A generator index that knows exactly the opcodes in the hand-built
/// fixtures below, plus one declared-unreachable opcode — so the three cell
/// states are all reachable from a corpus of one tiny class.
fn known_generators() -> GeneratorIndex {
    let mut g = GeneratorIndex::new();
    for op in [0x1a, 0x10, 0x60, 0xac] {
        g.insert(op, GeneratorStatus::Generated);
    }
    g.insert(
        0xa8,
        GeneratorStatus::Unreachable {
            reason: "javac has not emitted `jsr` since Java 5, and JVMS 4.9.1 forbids it in \
                     class files >= 50.0"
                .to_string(),
        },
    );
    g
}

#[test]
fn the_matrix_reports_a_known_corpus_correctly() {
    let class = class_with_code(&[0x1a, 0x10, 0x07, 0x60, 0xac], 0);
    let scan = scan_class_bytes("Known", &class).expect("well-formed");
    let modes = vec![Mode::NoJit, Mode::IrJit];
    let m = CoverageMatrix::build(vec![scan], &modes, Vec::new(), &known_generators());

    assert_eq!(m.schema_version, MATRIX_SCHEMA_VERSION);
    assert_eq!(m.modes, vec!["nojit", "ir-jit"]);
    assert_eq!(m.rows.len(), 4);
    assert_eq!(m.programs.len(), 1);

    let iadd = m.rows.iter().find(|r| r.opcode == 0x60).expect("iadd row");
    assert_eq!(iadd.name, "iadd");
    assert_eq!(iadd.form, "implicit");
    assert_eq!(iadd.programs, vec!["Known".to_string()]);
    // `nojit` drives interpreter-fast; `ir-jit` drives ir-jit. Neither
    // drives the decoded interpreter, the direct emitter, OSR or deopt.
    assert_eq!(iadd.axes["interpreter-fast"], true);
    assert_eq!(iadd.axes["ir-jit"], true);
    assert_eq!(iadd.axes["interpreter-decoded"], false);
    assert_eq!(iadd.axes["direct-jit"], false);
    assert_eq!(iadd.axes["osr"], false);
    assert_eq!(iadd.axes["deopt"], false);
    // The exception axis is mode-driven but code-gated: this method has no
    // handler and no athrow.
    assert_eq!(iadd.axes["exception"], false);

    assert!(m.fully_covered_rows() == 0);
    assert!(iadd.uncovered().contains(&"direct-jit"));

    // Mode-side gaps are named separately from code-side ones.
    assert_eq!(
        m.axes_without_a_mode,
        vec![
            "interpreter-decoded".to_string(),
            "direct-jit".to_string(),
            "osr".to_string(),
            "deopt".to_string()
        ]
    );

    // Everything the corpus does not contain is listed by name.
    assert!(m
        .unexercised_opcodes
        .iter()
        .any(|u| u.name == "invokedynamic"));
    assert!(!m.unexercised_opcodes.iter().any(|u| u.name == "iadd"));
    assert_eq!(m.unexercised_opcodes.len(), 202 - 4);

    // -- the per-opcode grid, exactly ----------------------------------
    // Every named opcode has a row, whether or not the corpus contains it.
    assert_eq!(m.opcodes.len(), 202);
    assert_eq!(m.cells.total(), 202 * PathAxis::all().len());

    let iadd = m.opcodes.iter().find(|o| o.opcode == 0x60).expect("iadd");
    assert_eq!(iadd.forms, vec!["implicit".to_string()]);
    assert_eq!(iadd.programs, vec!["Known".to_string()]);
    assert!(iadd.cells["interpreter-fast"].is_covered());
    assert!(iadd.cells["ir-jit"].is_covered());
    // Four axes have no mode; two are code-side gaps under the two that do.
    assert_eq!(
        iadd.cells["direct-jit"],
        CellState::uncovered(Gap::NoModeDrivesAxis)
    );
    assert_eq!(
        iadd.cells["osr"],
        CellState::uncovered(Gap::NoModeDrivesAxis)
    );
    assert_eq!(
        iadd.cells["exception"],
        CellState::uncovered(Gap::NoHandlerSite)
    );
    assert!(!iadd.fully_covered());

    // `jsr` is declared unreachable: every axis carries the reason, and it
    // is never mixed in with the ordinary gaps.
    let jsr = m.opcodes.iter().find(|o| o.opcode == 0xa8).expect("jsr");
    assert!(jsr.fully_unreachable());
    assert!(jsr.gaps().is_empty(), "unreachable is not a gap");
    match &jsr.cells["ir-jit"] {
        CellState::UnreachableByConstruction { reason } => assert!(reason.contains("4.9.1")),
        other => panic!("jsr must be unreachable, got {other:?}"),
    }
    assert_eq!(m.unreachable_opcodes(), 1);
    assert_eq!(m.cells.unreachable, PathAxis::all().len());

    // An opcode the generator claims but the corpus lacks is a *different*
    // gap from one nothing can produce — different fix, different owner.
    let ireturn_ok = m.opcodes.iter().find(|o| o.opcode == 0xac).unwrap();
    assert!(ireturn_ok.cells["interpreter-fast"].is_covered());
    let dup2_x2 = m.opcodes.iter().find(|o| o.opcode == 0x5e).unwrap();
    assert_eq!(
        dup2_x2.cells["interpreter-fast"],
        CellState::uncovered(Gap::NoGenerator)
    );

    // The gap render names the fix, not just the fact.
    let gaps = m.render_gaps();
    assert!(gaps.contains("fix: write a generator recipe"));
    assert!(gaps.contains("UNREACHABLE jsr"));
    assert!(m.render().contains("cells:"));
}

#[test]
fn an_opcode_with_no_generator_is_reported_uncovered_not_omitted() {
    // The failure this guards against: a report that only lists what it has
    // seen. With an empty generator index and an empty corpus, all 202
    // opcodes must still appear, each uncovered on every axis, each naming
    // "no generator" as the reason.
    let m = CoverageMatrix::build(
        Vec::new(),
        Mode::execution_paths(),
        Vec::new(),
        &GeneratorIndex::new(),
    );
    assert_eq!(m.opcodes.len(), 202);
    assert_eq!(m.fully_covered_opcodes(), 0);
    assert_eq!(m.unreachable_opcodes(), 0);
    assert_eq!(m.cells.uncovered, 202 * PathAxis::all().len());
    assert_eq!(m.cells.covered, 0);
    for report in &m.opcodes {
        assert!(report.generator.is_none());
        for (axis, gap) in report.gaps() {
            assert_eq!(gap, Gap::NoGenerator, "{} / {axis}", report.name);
            assert!(gap.fix().contains("opcorpus.rs"));
        }
    }
    // Including the ones that are unreachable *in practice*: without a
    // declaration the matrix must not invent one.
    let swap = m.opcodes.iter().find(|o| o.opcode == 0x5f).unwrap();
    assert!(!swap.fully_unreachable());
}

#[test]
fn corpus_evidence_overrides_an_unreachable_declaration() {
    // A mutated class really can carry a `swap`. If the bytes have it, the
    // report must say covered and flag the stale declaration, rather than
    // insisting the opcode cannot exist.
    let class = class_with_code(&[0x5f, 0xb1], 0);
    let scan = scan_class_bytes("Mutant", &class).expect("well-formed");
    let mut generators = GeneratorIndex::new();
    generators.insert(
        0x5f,
        GeneratorStatus::Unreachable {
            reason: "javac never emits swap".to_string(),
        },
    );
    let m = CoverageMatrix::build(vec![scan], Mode::execution_paths(), Vec::new(), &generators);
    let swap = m.opcodes.iter().find(|o| o.opcode == 0x5f).unwrap();
    assert!(swap.cells["interpreter-fast"].is_covered());
    assert!(!swap.fully_unreachable());
    assert_eq!(m.reconciliation.len(), 1);
    assert!(m.reconciliation[0].note.contains("stale"));
    assert!(m.render().contains("RECONCILE swap"));
}

#[test]
fn a_generator_whose_opcode_is_missing_from_the_corpus_is_reconciled() {
    // The other direction: the recipe stopped working (a javac lowering
    // changed), so the claim is louder than the evidence.
    let mut generators = GeneratorIndex::new();
    generators.insert(0xba, GeneratorStatus::Generated);
    let m = CoverageMatrix::build(Vec::new(), Mode::execution_paths(), Vec::new(), &generators);
    assert_eq!(m.reconciliation.len(), 1);
    assert_eq!(m.reconciliation[0].name, "invokedynamic");
    let indy = m.opcodes.iter().find(|o| o.opcode == 0xba).unwrap();
    assert_eq!(
        indy.cells["ir-jit"],
        CellState::uncovered(Gap::NotInCorpusButGenerated)
    );
    assert!(m.render_gaps().contains("fix: regenerate the corpus"));
}

#[test]
fn the_full_execution_path_list_drives_every_axis() {
    // The point of the mode axis: with the contract's `--modes` list, no
    // axis is a *mode-side* gap, so every remaining gap is the corpus's.
    let class = class_with_code(&[0x1a, 0x60, 0xac], 0);
    let scan = scan_class_bytes("K", &class).expect("well-formed");
    let m = CoverageMatrix::build(
        vec![scan],
        Mode::execution_paths(),
        Vec::new(),
        &GeneratorIndex::new(),
    );
    assert!(
        m.axes_without_a_mode.is_empty(),
        "execution_paths() must drive all seven axes, missing {:?}",
        m.axes_without_a_mode
    );
    // With every axis driven, no cell can carry a mode-side gap.
    for report in &m.opcodes {
        for (_, gap) in report.gaps() {
            assert_ne!(gap, Gap::NoModeDrivesAxis, "{}", report.name);
        }
    }
}

#[test]
fn the_matrix_round_trips_through_json() {
    let class = class_with_code(&[0x1a, 0x60, 0xac], 0);
    let scan = scan_class_bytes("K", &class).expect("well-formed");
    let m = CoverageMatrix::build(vec![scan], &[Mode::NoJit], Vec::new(), &known_generators());
    let json = m.to_json().expect("serialize");
    let back: CoverageMatrix = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.rows.len(), m.rows.len());
    assert_eq!(back.modes, m.modes);
    assert_eq!(back.opcodes.len(), m.opcodes.len());
    assert_eq!(back.cells, m.cells);
    assert_eq!(back.reconciliation, m.reconciliation);
    assert!(m.render().contains("opcode/path matrix"));
    assert!(m.render().contains("AXIS GAP"));
}

#[test]
fn unscannable_programs_are_surfaced_in_the_artifact() {
    let m = CoverageMatrix::build(
        Vec::new(),
        &[Mode::NoJit],
        vec![("Broken".into(), "not a well-formed class file".into())],
        &GeneratorIndex::new(),
    );
    assert_eq!(m.unscannable.len(), 1);
    assert!(m.render().contains("UNSCANNED Broken"));
    // A corpus that scanned nothing must not read as "everything covered".
    assert_eq!(m.rows.len(), 0);
    assert_eq!(m.unexercised_opcodes.len(), 202);
    assert_eq!(m.fully_covered_opcodes(), 0);
}

#[test]
fn axes_for_is_conservative_about_the_interpreter() {
    // Every JIT-enabled mode still interprets until a method is hot, so it
    // claims interpreter-fast. Only `interp-decoded` claims the decoded
    // fallback, and only it passes `--noverify`.
    for m in Mode::all() {
        let axes = axes_for(*m);
        if *m == Mode::InterpDecoded {
            assert!(
                axes.contains(&PathAxis::InterpreterDecoded),
                "{}",
                m.label()
            );
            assert!(!axes.contains(&PathAxis::InterpreterFast), "{}", m.label());
        } else {
            assert!(axes.contains(&PathAxis::InterpreterFast), "{}", m.label());
            assert!(
                !axes.contains(&PathAxis::InterpreterDecoded),
                "{} must not claim the decoded fallback",
                m.label()
            );
        }
    }
    assert!(axes_for(Mode::ForcedDeopt).contains(&PathAxis::Deopt));
    assert!(axes_for(Mode::OsrEager).contains(&PathAxis::Osr));
    assert!(!axes_for(Mode::NoOsr).contains(&PathAxis::Osr));
}
