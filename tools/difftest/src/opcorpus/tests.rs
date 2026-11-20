use super::*;
use crate::checksum;
use crate::matrix::scan_class_bytes;

fn all() -> Vec<OpcodeProgram> {
    generate_all(&OpcodeCorpusConfig::default())
}

/// The JVMS 6.5 opcode table, taken from OUTSIDE this crate.
///
/// WHY THIS EXISTS. Until 2026-08-13
/// `there_is_an_entry_for_every_named_opcode_and_nothing_else` asserted
/// `p.mnemonic == matrix::opcode_name(p.opcode)` — but `generate_all` builds
/// the corpus as `(0..=0xc9).filter_map(matrix::opcode_name)`, so the corpus
/// and the assertion were the same expression evaluated twice. A misspelled
/// mnemonic (`invokedyanmic`), a swapped `dup_x2`/`dup2_x1`, or any wrong
/// opcode-to-name mapping in `matrix::NAMES` was invisible: the corpus
/// carried the wrong name and the test agreed with it. The `len() == 202`
/// half was already a compile-time fact of `const NAMES: [&str; 202]`.
/// (E25 sweep,
/// `docs/feature-designs/differential-fuzzer.md`
/// section 4.1, row 34.)
///
/// PROVENANCE — two independent JDK 25 sources, cross-checked against each
/// other on 2026-08-13, neither of which is `matrix::opcode_name`:
///
/// 1. `jdk.hotspot.agent/sun/jvm/hotspot/interpreter/Bytecodes.java` from a
///    JDK 25 source checkout (`C:\craton\jdk25src` on this host) — the
///    Serviceability Agent's bytecode table. Every `def(_x, "x", ...)` row
///    joined to its `public static final int _x = N;` value; 202 rows with
///    value <= 0xc9. That join is the table below, verbatim.
/// 2. `java.lang.classfile.Opcode` (JEP 484 ClassFile API), enumerated by
///    running `Opcode.values()` on
///    `openjdk 25.0.3 2026-04-21 LTS, Microsoft-13877124, build 25.0.3+9-LTS`
///    on this host — skipping `isWide()` pseudo-opcodes, keyed by
///    `bytecode()`, lowercased. 201 rows.
///
/// The two agree on all 201 opcodes they share. Source 2 has no entry for
/// `0xc4 wide`, because the ClassFile API models `wide` as a modifier on the
/// widened opcode rather than as an opcode of its own; source 1 does, JVMS
/// 6.5 does, and this crate's decoder must, so the table keeps it.
///
/// RESIDUAL: a transcribed table carries the "JDK 26 changes it" risk, and
/// nothing re-derives this one at test time. The risk is small — the last
/// assignment added below `0xca` was `0xba invokedynamic` in Java 7 — but it
/// is not zero. Re-run the two extractions on a JDK bump.
#[rustfmt::skip]
const JVMS_MNEMONICS: &[(u8, &str)] = &[
(0x00, "nop"), (0x01, "aconst_null"), (0x02, "iconst_m1"), (0x03, "iconst_0"),
(0x04, "iconst_1"), (0x05, "iconst_2"), (0x06, "iconst_3"), (0x07, "iconst_4"),
(0x08, "iconst_5"), (0x09, "lconst_0"), (0x0a, "lconst_1"), (0x0b, "fconst_0"),
(0x0c, "fconst_1"), (0x0d, "fconst_2"), (0x0e, "dconst_0"), (0x0f, "dconst_1"),
(0x10, "bipush"), (0x11, "sipush"), (0x12, "ldc"), (0x13, "ldc_w"), (0x14, "ldc2_w"),
(0x15, "iload"), (0x16, "lload"), (0x17, "fload"), (0x18, "dload"), (0x19, "aload"),
(0x1a, "iload_0"), (0x1b, "iload_1"), (0x1c, "iload_2"), (0x1d, "iload_3"),
(0x1e, "lload_0"), (0x1f, "lload_1"), (0x20, "lload_2"), (0x21, "lload_3"),
(0x22, "fload_0"), (0x23, "fload_1"), (0x24, "fload_2"), (0x25, "fload_3"),
(0x26, "dload_0"), (0x27, "dload_1"), (0x28, "dload_2"), (0x29, "dload_3"),
(0x2a, "aload_0"), (0x2b, "aload_1"), (0x2c, "aload_2"), (0x2d, "aload_3"),
(0x2e, "iaload"), (0x2f, "laload"), (0x30, "faload"), (0x31, "daload"), (0x32, "aaload"),
(0x33, "baload"), (0x34, "caload"), (0x35, "saload"), (0x36, "istore"), (0x37, "lstore"),
(0x38, "fstore"), (0x39, "dstore"), (0x3a, "astore"), (0x3b, "istore_0"),
(0x3c, "istore_1"), (0x3d, "istore_2"), (0x3e, "istore_3"), (0x3f, "lstore_0"),
(0x40, "lstore_1"), (0x41, "lstore_2"), (0x42, "lstore_3"), (0x43, "fstore_0"),
(0x44, "fstore_1"), (0x45, "fstore_2"), (0x46, "fstore_3"), (0x47, "dstore_0"),
(0x48, "dstore_1"), (0x49, "dstore_2"), (0x4a, "dstore_3"), (0x4b, "astore_0"),
(0x4c, "astore_1"), (0x4d, "astore_2"), (0x4e, "astore_3"), (0x4f, "iastore"),
(0x50, "lastore"), (0x51, "fastore"), (0x52, "dastore"), (0x53, "aastore"),
(0x54, "bastore"), (0x55, "castore"), (0x56, "sastore"), (0x57, "pop"), (0x58, "pop2"),
(0x59, "dup"), (0x5a, "dup_x1"), (0x5b, "dup_x2"), (0x5c, "dup2"), (0x5d, "dup2_x1"),
(0x5e, "dup2_x2"), (0x5f, "swap"), (0x60, "iadd"), (0x61, "ladd"), (0x62, "fadd"),
(0x63, "dadd"), (0x64, "isub"), (0x65, "lsub"), (0x66, "fsub"), (0x67, "dsub"),
(0x68, "imul"), (0x69, "lmul"), (0x6a, "fmul"), (0x6b, "dmul"), (0x6c, "idiv"),
(0x6d, "ldiv"), (0x6e, "fdiv"), (0x6f, "ddiv"), (0x70, "irem"), (0x71, "lrem"),
(0x72, "frem"), (0x73, "drem"), (0x74, "ineg"), (0x75, "lneg"), (0x76, "fneg"),
(0x77, "dneg"), (0x78, "ishl"), (0x79, "lshl"), (0x7a, "ishr"), (0x7b, "lshr"),
(0x7c, "iushr"), (0x7d, "lushr"), (0x7e, "iand"), (0x7f, "land"), (0x80, "ior"),
(0x81, "lor"), (0x82, "ixor"), (0x83, "lxor"), (0x84, "iinc"), (0x85, "i2l"),
(0x86, "i2f"), (0x87, "i2d"), (0x88, "l2i"), (0x89, "l2f"), (0x8a, "l2d"), (0x8b, "f2i"),
(0x8c, "f2l"), (0x8d, "f2d"), (0x8e, "d2i"), (0x8f, "d2l"), (0x90, "d2f"), (0x91, "i2b"),
(0x92, "i2c"), (0x93, "i2s"), (0x94, "lcmp"), (0x95, "fcmpl"), (0x96, "fcmpg"),
(0x97, "dcmpl"), (0x98, "dcmpg"), (0x99, "ifeq"), (0x9a, "ifne"), (0x9b, "iflt"),
(0x9c, "ifge"), (0x9d, "ifgt"), (0x9e, "ifle"), (0x9f, "if_icmpeq"), (0xa0, "if_icmpne"),
(0xa1, "if_icmplt"), (0xa2, "if_icmpge"), (0xa3, "if_icmpgt"), (0xa4, "if_icmple"),
(0xa5, "if_acmpeq"), (0xa6, "if_acmpne"), (0xa7, "goto"), (0xa8, "jsr"), (0xa9, "ret"),
(0xaa, "tableswitch"), (0xab, "lookupswitch"), (0xac, "ireturn"), (0xad, "lreturn"),
(0xae, "freturn"), (0xaf, "dreturn"), (0xb0, "areturn"), (0xb1, "return"),
(0xb2, "getstatic"), (0xb3, "putstatic"), (0xb4, "getfield"), (0xb5, "putfield"),
(0xb6, "invokevirtual"), (0xb7, "invokespecial"), (0xb8, "invokestatic"),
(0xb9, "invokeinterface"), (0xba, "invokedynamic"), (0xbb, "new"), (0xbc, "newarray"),
(0xbd, "anewarray"), (0xbe, "arraylength"), (0xbf, "athrow"), (0xc0, "checkcast"),
(0xc1, "instanceof"), (0xc2, "monitorenter"), (0xc3, "monitorexit"), (0xc4, "wide"),
(0xc5, "multianewarray"), (0xc6, "ifnull"), (0xc7, "ifnonnull"), (0xc8, "goto_w"),
(0xc9, "jsr_w"),
];

/// `matrix::opcode_name` answers the JVMS, not itself.
///
/// This is the test that makes a typo in `matrix::NAMES` reportable. It
/// compares the whole `u8` domain, not the corpus's own range, so a name
/// appearing where the JVMS has a reserved opcode is caught too.
#[test]
fn the_matrix_opcode_table_is_the_jvms_opcode_table() {
    assert_eq!(
        JVMS_MNEMONICS.len(),
        202,
        "the external table lost rows in transcription"
    );
    for (i, (op, _)) in JVMS_MNEMONICS.iter().enumerate() {
        assert_eq!(
            *op as usize, i,
            "the external table is not dense and in opcode order at index {i}"
        );
    }
    let mut wrong: Vec<String> = Vec::new();
    for op in 0u8..=0xff {
        let jvms = JVMS_MNEMONICS
            .iter()
            .find(|(o, _)| *o == op)
            .map(|(_, name)| *name);
        let ours = matrix::opcode_name(op);
        if ours != jvms {
            wrong.push(format!("{op:#04x}: matrix={ours:?} JVMS={jvms:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "matrix::opcode_name disagrees with the JDK 25 opcode table:\n  {}\n\n\
         The JVMS table above came from the Serviceability Agent's \
         Bytecodes.java and from java.lang.classfile.Opcode; if a JDK bump \
         genuinely changed an assignment, re-derive it (see the const's \
         PROVENANCE) rather than editing a row to match this crate.",
        wrong.join("\n  ")
    );
}

#[test]
fn there_is_an_entry_for_every_named_opcode_and_nothing_else() {
    let programs = all();
    // The expectation is the EXTERNAL table's population, not this crate's.
    let named_in_range = JVMS_MNEMONICS.iter().filter(|(op, _)| *op <= 0xc9).count();
    assert_eq!(
        programs.len(),
        named_in_range,
        "one entry per JVMS mnemonic in 0x00..=0xc9"
    );
    for (idx, p) in programs.iter().enumerate() {
        assert_eq!(p.opcode as usize, idx, "entries are in opcode order");
        let jvms = JVMS_MNEMONICS
            .iter()
            .find(|(o, _)| *o == p.opcode)
            .map(|(_, name)| *name);
        assert_eq!(
            Some(p.mnemonic),
            jvms,
            "corpus entry {idx} carries a mnemonic the JVMS does not give \
             opcode {:#04x}. Before 2026-08-13 this compared against \
             matrix::opcode_name, which is where the corpus got the name \
             from, so it could not fail.",
            p.opcode
        );
    }
}

#[test]
fn the_generator_emits_a_program_per_opcode_and_the_walker_rediscovers_it() {
    // The load-bearing test: for every opcode this build claims to
    // generate, the matrix walker finds *that* opcode in the program's
    // witness. Hermetic on purpose — no javac, so a CI worker without a JDK
    // still fails the day a recipe and the walker disagree.
    let mut generated = 0usize;
    for p in all() {
        if !p.is_generated() {
            continue;
        }
        generated += 1;
        let class = witness_class(&p.witness, 1);
        let scan = scan_class_bytes(&p.class_name, &class)
            .unwrap_or_else(|| panic!("{} witness is not a walkable class", p.class_name));
        assert!(
            scan.sites.keys().any(|k| k
                .split_once(':')
                .and_then(|(op, _)| op.parse::<u8>().ok())
                == Some(p.opcode)),
            "{} ({}): the walker did not rediscover opcode {:#04x} in its witness; sites = {:?}",
            p.class_name,
            p.mnemonic,
            p.opcode,
            scan.sites.keys().collect::<Vec<_>>()
        );
    }
    assert_eq!(
        generated,
        202 - 5,
        "five opcodes are unreachable from Java source; everything else is generated"
    );
}

#[test]
fn every_witness_walks_to_exactly_its_own_length() {
    // A witness the walker can only partially decode would make the test
    // above pass for the wrong reason (the focus opcode is first).
    for p in all() {
        let mut pc = 0usize;
        while pc < p.witness.len() {
            let len = matrix::instruction_len(&p.witness, pc).unwrap_or_else(|| {
                panic!("{}: undecodable at pc {pc}", p.class_name);
            });
            pc += len;
        }
        assert_eq!(pc, p.witness.len(), "{}: overran its witness", p.class_name);
        assert!(!p.forms.is_empty(), "{}: no derived form", p.class_name);
    }
}

#[test]
fn the_operand_forms_the_recipes_target_are_present() {
    let by_op: std::collections::BTreeMap<u8, OpcodeProgram> =
        all().into_iter().map(|p| (p.opcode, p)).collect();
    // newarray covers every atype: a boolean[] and a double[] are different
    // paths in all four executors, so one `newarray` row would be a lie.
    let newarray = &by_op[&0xbc].forms;
    for ty in [
        "boolean", "char", "float", "double", "byte", "short", "int", "long",
    ] {
        assert!(
            newarray.contains(&format!("atype:{ty}")),
            "newarray witness is missing atype:{ty}"
        );
    }
    assert!(by_op[&0xc4].forms.contains(&"wide:iinc".to_string()));
    assert!(by_op[&0xc4].forms.contains(&"wide:iload".to_string()));
    assert!(by_op[&0xc5].forms.contains(&"cp16,dims:3".to_string()));
    assert!(by_op[&0xaa].forms.contains(&"table".to_string()));
    assert!(by_op[&0xab].forms.contains(&"lookup".to_string()));
    assert!(by_op[&0xc8].forms.contains(&"branch:s32".to_string()));
}

#[test]
fn every_generated_program_declares_a_checksum() {
    for p in all() {
        let Some(source) = &p.source else { continue };
        assert!(
            source.contains(&format!("{} {} ", checksum::MARKER, p.checksum_name)),
            "{} declares no checksum under {}",
            p.class_name,
            p.checksum_name
        );
        // And the name must survive the extractor. A marker whose name is
        // empty or whitespace is dropped silently by `checksum::extract`,
        // which would leave the dimension exactly as vacuous as having no
        // marker at all — the failure this corpus exists to end.
        let emitted = format!("{} {} deadbeefdeadbeef", checksum::MARKER, p.checksum_name);
        assert_eq!(
            checksum::extract(&emitted)
                .get(&p.checksum_name)
                .map(String::as_str),
            Some("deadbeefdeadbeef"),
            "{}: the declared checksum name does not round-trip",
            p.class_name
        );
    }
}

#[test]
fn generation_is_deterministic() {
    let config = OpcodeCorpusConfig::default();
    assert_eq!(generate_all(&config), generate_all(&config));
    // And sensitive to its inputs, so the config is really an input rather
    // than decoration.
    let other = OpcodeCorpusConfig {
        loop_iterations: 7,
        ..OpcodeCorpusConfig::default()
    };
    assert_ne!(generate_all(&config), generate_all(&other));
}

#[test]
fn generated_sources_are_well_formed_and_self_naming() {
    for p in all() {
        let Some(source) = &p.source else { continue };
        assert!(
            source.contains(&format!("public class {}", p.class_name)),
            "{}: class declaration does not match the file stem",
            p.class_name
        );
        assert!(source.contains("public static void main(String[] args)"));
        assert!(
            !source.contains('@'),
            "{}: an unreplaced template placeholder survived",
            p.class_name
        );
        assert!(balanced(source), "{}: unbalanced braces", p.class_name);
        assert!(
            source.starts_with("// difftest: strict"),
            "{}: missing the strict pragma header",
            p.class_name
        );
    }
}

/// The opcodes whose focus instruction cannot sit in `main` — it needs a
/// parameter pinned to a particular slot, a particular return type, more
/// locals than a byte can index, or a method body too large to branch
/// across. For those the loop and the handler have to be *inside the
/// helper*, because [`matrix::SiteFacts`] do not cross a method boundary.
const HELPER_FOCUSED: &[u8] = &[
    0x13, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22,
    0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x36, 0x37, 0x38, 0x39,
    0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0xac, 0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xc4, 0xc8,
];

#[test]
fn programs_wrap_their_focus_in_a_loop_and_a_handler() {
    // The `osr` and `exception` code sides are per method. If this ever
    // stops holding, those two columns quietly empty out while the opcode
    // count keeps rising, which is exactly the misleading number the matrix
    // exists to prevent.
    for p in all() {
        let Some(source) = &p.source else { continue };
        assert!(
            source.contains("for (int i = 0; i < trips; i++)"),
            "{}: main has no back-edge",
            p.class_name
        );
        assert!(
            source.contains("catch (Throwable caught)"),
            "{}: main has no handler",
            p.class_name
        );
        if HELPER_FOCUSED.contains(&p.opcode) {
            assert!(
                source.contains("for (int k = 0; k < 3; k++)"),
                "{}: the focus is in a helper, which has no back-edge of its own",
                p.class_name
            );
            assert!(
                source.contains("catch (RuntimeException ex)"),
                "{}: the focus is in a helper, which has no handler of its own",
                p.class_name
            );
        }
    }
}

#[test]
fn the_helper_builder_always_emits_a_loop_and_a_handler() {
    let h = helper("h", "int p", "int a = p;", "s += a;");
    assert!(h.contains("for (int k = 0; k < 3; k++)"));
    assert!(h.contains("catch (RuntimeException ex)"));
    assert!(
        h.contains("    int a = p;"),
        "decls are indented into place"
    );
    assert!(balanced(&h), "helper braces: {h}");
}

#[test]
fn the_five_unreachable_opcodes_say_why() {
    let by_op: std::collections::BTreeMap<u8, OpcodeProgram> =
        all().into_iter().map(|p| (p.opcode, p)).collect();
    for op in [0x00u8, 0x5f, 0xa8, 0xa9, 0xc9] {
        let p = &by_op[&op];
        assert!(p.source.is_none(), "{} must not be generated", p.mnemonic);
        let reason = p
            .support
            .reason()
            .unwrap_or_else(|| panic!("{} has no reason", p.mnemonic));
        assert!(
            reason.len() > 40,
            "{}: reason is not actionable",
            p.mnemonic
        );
    }
    // Everything else is generated.
    for p in by_op.values() {
        if matches!(p.opcode, 0x00 | 0x5f | 0xa8 | 0xa9 | 0xc9) {
            continue;
        }
        assert!(p.is_generated(), "{} should be generated", p.mnemonic);
    }
}

#[test]
fn the_large_recipes_are_sized_by_config_and_can_be_skipped() {
    let big = OpcodeCorpusConfig::default();
    let by_op: std::collections::BTreeMap<u8, OpcodeProgram> = generate_all(&big)
        .into_iter()
        .map(|p| (p.opcode, p))
        .collect();
    // goto_w needs a >32 KiB method: ~4 bytes per statement.
    let goto_w = by_op[&0xc8].source.as_ref().expect("goto_w generated");
    assert!(goto_w.matches("s += 1;").count() >= 9_000);
    // wide needs >255 locals.
    let wide = by_op[&0xc4].source.as_ref().expect("wide generated");
    assert!(wide.contains("int v299 = p + 299;"));
    assert!(wide.contains("v299 += 1000;"));

    let small = OpcodeCorpusConfig {
        include_large: false,
        ..OpcodeCorpusConfig::default()
    };
    let skipped: Vec<u8> = generate_all(&small)
        .into_iter()
        .filter(|p| matches!(p.support, OpcodeSupport::Skipped(_)))
        .map(|p| p.opcode)
        .collect();
    assert_eq!(skipped, vec![0xc4, 0xc8]);
}

#[test]
fn the_generator_index_carries_reasons_into_the_matrix() {
    let index = generator_index(&OpcodeCorpusConfig::default());
    assert_eq!(index.len(), 202);
    assert_eq!(index[&0x60], GeneratorStatus::Generated);
    match &index[&0xa8] {
        GeneratorStatus::Unreachable { reason } => assert!(reason.contains("jsr")),
        other => panic!("jsr must be unreachable, got {other:?}"),
    }
}

#[test]
fn a_witness_class_reports_the_handler_and_loop_facts_the_walker_needs() {
    // `witness_class(.., 1)` must give the scanned sites a handler, and a
    // backward branch must give them a loop — the two code-side gates.
    let scan = scan_class_bytes("W", &witness_class(&[0x60, 0xb1], 1)).expect("walkable");
    assert!(scan.sites.values().all(|f| f.in_exception_method));
    let looped = scan_class_bytes("L", &witness_class(&[0x1a, 0x9a, 0xff, 0xfe, 0xb1], 0))
        .expect("walkable");
    assert!(looped.sites.values().all(|f| f.in_loop_method));
}

/// Brace balance over generated Java, ignoring comment lines and the
/// contents of string and char literals. Comment lines are dropped whole
/// because an apostrophe in prose ("javac's") would otherwise open a char
/// literal that never closes and silently disable the rest of the check.
fn balanced(s: &str) -> bool {
    let mut depth = 0i32;
    for line in s.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let mut in_string = false;
        let mut in_char = false;
        let mut prev = '\0';
        for ch in line.chars() {
            match ch {
                '"' if !in_char && prev != '\\' => in_string = !in_string,
                '\'' if !in_string && prev != '\\' => in_char = !in_char,
                '{' if !in_string && !in_char => depth += 1,
                '}' if !in_string && !in_char => depth -= 1,
                _ => {}
            }
            if depth < 0 {
                return false;
            }
            prev = ch;
        }
    }
    depth == 0
}
