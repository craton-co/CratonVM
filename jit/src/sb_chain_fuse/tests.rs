use super::*;

const SB_INIT: u16 = 1;
const APPEND_S: u16 = 2;
const APPEND_C: u16 = 3;
const APPEND_I: u16 = 4;
const APPEND_J: u16 = 5;
const TO_STRING: u16 = 6;
const APPEND_O: u16 = 7;
const SET_LENGTH: u16 = 8;
const INSERT: u16 = 9;
const OTHER_CALL: u16 = 10;
const OTHER_INIT: u16 = 11;
const SB_INIT_STR: u16 = 12;
const APPEND_Z: u16 = 13;
const SB_INIT_INT: u16 = 14;
const APPEND_F: u16 = 15;
const APPEND_D: u16 = 16;

fn member(cp: u16) -> Option<(String, String, String)> {
    let (c, n, d) = match cp {
        SB_INIT => (SB_CLASS, "<init>", "()V"),
        SB_INIT_STR => (SB_CLASS, "<init>", "(Ljava/lang/String;)V"),
        APPEND_S => (
            SB_CLASS,
            "append",
            "(Ljava/lang/String;)Ljava/lang/StringBuilder;",
        ),
        APPEND_C => (SB_CLASS, "append", "(C)Ljava/lang/StringBuilder;"),
        APPEND_I => (SB_CLASS, "append", "(I)Ljava/lang/StringBuilder;"),
        APPEND_J => (SB_CLASS, "append", "(J)Ljava/lang/StringBuilder;"),
        APPEND_Z => (SB_CLASS, "append", "(Z)Ljava/lang/StringBuilder;"),
        SB_INIT_INT => (SB_CLASS, "<init>", "(I)V"),
        APPEND_F => (SB_CLASS, "append", "(F)Ljava/lang/StringBuilder;"),
        APPEND_D => (SB_CLASS, "append", "(D)Ljava/lang/StringBuilder;"),
        APPEND_O => (
            SB_CLASS,
            "append",
            "(Ljava/lang/Object;)Ljava/lang/StringBuilder;",
        ),
        TO_STRING => (SB_CLASS, "toString", "()Ljava/lang/String;"),
        SET_LENGTH => (SB_CLASS, "setLength", "(I)V"),
        INSERT => (SB_CLASS, "insert", "(IC)Ljava/lang/StringBuilder;"),
        OTHER_CALL => ("java/lang/Integer", "intValue", "()I"),
        OTHER_INIT => ("java/lang/Object", "<init>", "()V"),
        _ => return None,
    };
    Some((c.to_string(), n.to_string(), d.to_string()))
}

/// A tiny assembler: `new SB(cp 20); dup; invokespecial <init>`.
struct Asm(Vec<u8>);

impl Asm {
    fn new() -> Self {
        Asm(Vec::new())
    }
    fn op(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }
    fn invoke(self, opcode: u8, cp: u16) -> Self {
        let [hi, lo] = cp.to_be_bytes();
        self.op(&[opcode, hi, lo])
    }
    fn sb_new(self) -> Self {
        self.op(&[0xbb, 0, 20, 0x59]).invoke(0xb7, SB_INIT)
    }
    fn append(self, cp: u16) -> Self {
        self.invoke(0xb6, cp)
    }
    fn sb_to_string(self) -> Self {
        self.invoke(0xb6, TO_STRING)
    }
}

fn recognize_at(
    code: &[u8],
    new_pc: usize,
    handlers: &[(usize, usize, usize)],
) -> Result<SbChain, &'static str> {
    let targets = crate::bytecode_analysis::branch_target_map(code, code.len());
    let src = SbChainCode {
        code,
        branch_targets: &targets,
        handlers,
    };
    let invoke_at = |pc: usize| member(u16::from_be_bytes([code[pc + 1], code[pc + 2]]));
    // `ldc #30` is the String "k", `ldc #31` an int constant.
    let ldc_string_at = |pc: usize| match code[pc] {
        0x12 if code[pc + 1] == 30 => Some("k".encode_utf16().collect()),
        0x13 if code[pc + 2] == 30 => Some("k".encode_utf16().collect()),
        _ => None,
    };
    let field_at = |cp: u16| (cp == 40).then_some(b'L');
    recognize_sb_chain(&src, new_pc, &invoke_at, &ldc_string_at, &field_at)
}

fn recognize(code: &[u8]) -> Result<SbChain, &'static str> {
    recognize_at(code, 0, &[])
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[test]
fn boolean_append_is_fused_as_a_boolean_concat_argument() {
    let code = Asm::new()
        .sb_new()
        .op(&[0x12, 30]) // ldc "k"
        .append(APPEND_S)
        .op(&[0x1a]) // iload_0
        .append(APPEND_Z)
        .sb_to_string()
        .op(&[0xb0])
        .0;
    let chain = recognize(&code).expect("boolean append is supported");
    let plan = chain.recipe().plan().expect("inline concat plan");
    assert_eq!(plan.arg_count, 1);
    assert!(matches!(
        plan.segments.as_slice(),
        [
            crate::concat_inline_plan::ConcatSegment::Literal(_),
            crate::concat_inline_plan::ConcatSegment::Arg {
                index: 0,
                kind: crate::concat_inline_plan::ConcatArgKind::Boolean
            }
        ]
    ));
}

/// sbf21: `new StringBuilder().append(b).toString()` is a fresh String,
/// but a recipe of exactly one `boolean` operand and no constant is the
/// concat bridge's unary case, which answers the interned "true" /
/// "false" literal (`unary_boolean_concat_operand`): refused, unless a
/// constant segment keeps it an ordinary concat.
#[test]
fn sbf21_a_lone_boolean_append_is_refused_but_not_with_a_constant() {
    let lone = Asm::new()
        .sb_new()
        .op(&[0x1a]) // iload_0
        .append(APPEND_Z)
        .sb_to_string()
        .op(&[0xb0])
        .0;
    assert!(recognize(&lone).unwrap_err().contains("lone boolean"));
    // A non-empty literal beside it.
    let with_literal = Asm::new()
        .sb_new()
        .op(&[0x1a])
        .append(APPEND_Z)
        .op(&[0x12, 30])
        .append(APPEND_S)
        .sb_to_string()
        .op(&[0xb0])
        .0;
    assert!(recognize(&with_literal).is_ok());
    // Two boolean operands are not the unary shape either.
    let two = Asm::new()
        .sb_new()
        .op(&[0x1a])
        .append(APPEND_Z)
        .op(&[0x1b])
        .append(APPEND_Z)
        .sb_to_string()
        .op(&[0xb0])
        .0;
    assert!(recognize(&two).is_ok());
}

#[test]
fn a_literal_string_constructor_becomes_the_first_concat_constant() {
    let code = Asm::new()
        .op(&[0xbb, 0, 20, 0x59, 0x12, 30]) // new; dup; ldc "k"
        .invoke(0xb7, SB_INIT_STR)
        .op(&[0x1a]) // iload_0
        .append(APPEND_Z)
        .sb_to_string()
        .op(&[0xb0])
        .0;
    let chain = recognize(&code).expect("literal String constructor is supported");
    assert_eq!(chain.init_pc(), 6);
    assert_eq!(chain.first_operand_pc(), 9);
    assert_eq!(chain.constructor_literal.as_deref(), Some(&units("k")[..]));
    let recipe = chain.recipe();
    assert_eq!(recipe.recipe, vec![CONCAT_TAG_CONST, CONCAT_TAG_ARG]);
    assert_eq!(recipe.constants, vec![units("k")]);
    assert_eq!(recipe.descriptor, "(Z)Ljava/lang/String;");
    assert!(recipe.plan().is_some());
}

#[test]
fn field_and_array_reads_are_admitted_as_reexecuting_operands() {
    let field = Asm::new()
        .sb_new()
        .op(&[0x2a, 0xb4, 0, 40]) // aload_0; getfield #40
        .append(APPEND_S)
        .sb_to_string()
        .op(&[0xb0])
        .0;
    let chain = recognize(&field).expect("instance field operand is supported");
    assert!(matches!(
        chain.parts.as_slice(),
        [SbChainPart::Value {
            kind: SbAppendKind::String,
            operand_pc: 7,
            ..
        }]
    ));

    let array = Asm::new()
        .sb_new()
        .op(&[0x2a, 0x03, 0x32]) // aload_0; iconst_0; aaload
        .append(APPEND_S)
        .sb_to_string()
        .op(&[0xb0])
        .0;
    let chain = recognize(&array).expect("array element operand is supported");
    assert!(matches!(
        chain.parts.as_slice(),
        [SbChainPart::Value {
            kind: SbAppendKind::String,
            operand_pc: 7,
            ..
        }]
    ));
}

/// `R21StrintrRows.sbChunk`'s chain:
/// `new SB().append("k").append(i).append('-').append(i * 7).toString()`.
fn sb_chunk() -> Vec<u8> {
    Asm::new()
        .sb_new()
        .op(&[0x12, 30]) // ldc "k"
        .append(APPEND_S)
        .op(&[0x1b]) // iload_1
        .append(APPEND_I)
        .op(&[0x10, 45]) // bipush '-'
        .append(APPEND_C)
        .op(&[0x1b, 0x10, 7, 0x68]) // iload_1; bipush 7; imul
        .append(APPEND_I)
        .sb_to_string()
        .op(&[0xb0]) // areturn
        .0
}

#[test]
fn the_sb_chunk_chain_is_one_recipe_with_folded_literals() {
    let code = sb_chunk();
    let chain = recognize(&code).expect("admitted");
    assert_eq!(chain.new_pc, 0);
    assert_eq!(chain.parts.len(), 4);
    assert_eq!(
        chain.parts[0],
        SbChainPart::Literal {
            text: units("k"),
            operand_pc: 7,
            append_pc: 9
        }
    );
    assert_eq!(
        chain.parts[1],
        SbChainPart::Value {
            kind: SbAppendKind::Int,
            operand_pc: 12,
            append_pc: 13
        }
    );
    assert_eq!(
        chain.parts[2],
        SbChainPart::Literal {
            text: units("-"),
            operand_pc: 16,
            append_pc: 18
        }
    );
    assert_eq!(
        chain.parts[3],
        SbChainPart::Value {
            kind: SbAppendKind::Int,
            operand_pc: 21,
            append_pc: 25
        }
    );
    assert_eq!(chain.to_string_pc, 28);
    assert_eq!(chain.end_pc(), 31);
    assert_eq!(chain.value_count(), 2);
    assert_eq!(chain.invoke_pcs(), vec![4, 9, 13, 18, 25, 28]);
    let r = chain.recipe();
    assert_eq!(
        r.recipe,
        vec![
            CONCAT_TAG_CONST,
            CONCAT_TAG_ARG,
            CONCAT_TAG_CONST,
            CONCAT_TAG_ARG
        ]
    );
    assert_eq!(r.constants, vec![units("k"), units("-")]);
    assert_eq!(r.descriptor, "(II)Ljava/lang/String;");
    let plan = r.plan().expect("the inline plan admits it");
    assert_eq!(plan.arg_count, 2);
    assert_eq!(plan.literal_len, 2);
    assert!(!plan.wide);
    assert!(chain.contains_pc(9) && chain.contains_pc(30));
    assert!(!chain.contains_pc(0) && !chain.contains_pc(31));
}

#[test]
fn every_admitted_operand_kind_and_constant_folds() {
    let code = Asm::new()
        .sb_new()
        .op(&[0x2b]) // aload_1 (a String)
        .append(APPEND_S)
        .op(&[0x1c, 0x10, 0x61, 0x60, 0x92]) // (char) (iload_2 + 'a')
        .append(APPEND_C)
        .op(&[0x20]) // lload_2
        .append(APPEND_J)
        .op(&[0x1b, 0x85, 0x0a, 0x61]) // (long) i + 1L
        .append(APPEND_J)
        .op(&[0x0a]) // lconst_1
        .append(APPEND_J)
        .op(&[0x11, 0x01, 0x00]) // sipush 256 as int
        .append(APPEND_I)
        .op(&[0x11, 0x04, 0x30]) // sipush 0x430 as char (UTF16 literal)
        .append(APPEND_C)
        .op(&[0x02]) // iconst_m1 as int
        .append(APPEND_I)
        .sb_to_string()
        .0;
    let chain = recognize(&code).expect("admitted");
    let kinds: Vec<String> = chain
        .parts
        .iter()
        .map(|p| match p {
            SbChainPart::Literal { text, .. } => {
                format!("lit:{}", String::from_utf16_lossy(text))
            }
            SbChainPart::Value { kind, .. } => format!("{kind:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "String",
            "Char",
            "Long",
            "Long",
            "lit:1",
            "lit:256",
            "lit:\u{430}",
            "lit:-1"
        ]
    );
    let r = chain.recipe();
    assert_eq!(r.descriptor, "(Ljava/lang/String;CJJ)Ljava/lang/String;");
    // A literal above 0xFF: a wide plan (UTF16 result), still admitted.
    assert!(r.plan().expect("admitted").wide);
}

#[test]
fn a_ldc_w_literal_and_a_chain_mid_method_are_admitted() {
    let code = Asm::new()
        .op(&[0x1a, 0x57]) // iload_0; pop  (code before the chain)
        .sb_new()
        .op(&[0x13, 0, 30]) // ldc_w "k"
        .append(APPEND_S)
        .op(&[0x1a])
        .append(APPEND_I)
        .sb_to_string()
        .op(&[0xb0])
        .0;
    let chain = recognize_at(&code, 2, &[]).expect("admitted");
    assert_eq!(chain.new_pc, 2);
    assert!(matches!(chain.parts[0], SbChainPart::Literal { .. }));
    // The same code under a handler that covers all of it, or none of it.
    assert!(recognize_at(&code, 2, &[(0, code.len(), code.len() - 1)]).is_ok());
    assert!(recognize_at(&code, 2, &[(0, 2, code.len() - 1)]).is_ok());
}

#[test]
fn a_non_escaping_local_builder_chain_is_admitted() {
    let code = Asm::new()
        .sb_new()
        .op(&[0x4c]) // astore_1
        .op(&[0x2b, 0x12, 30]) // aload_1; ldc "k"
        .append(APPEND_S)
        .op(&[0x57, 0x2b, 0x1a]) // pop; aload_1; iload_0
        .append(APPEND_I)
        .op(&[0x57, 0x2b]) // pop; aload_1
        .sb_to_string()
        .op(&[0xb0]) // areturn
        .0;
    let chain = recognize(&code).expect("local builder does not escape");
    assert_eq!(chain.parts.len(), 2);
    assert_eq!(chain.to_string_pc, 22);
    assert_eq!(chain.invoke_pcs(), vec![4, 11, 17, 22]);
}

#[test]
fn a_chained_local_builder_uses_the_append_result_as_receiver() {
    let code = Asm::new()
        .sb_new()
        .op(&[0x4c]) // astore_1
        .op(&[0x2b, 0x12, 30]) // aload_1; ldc "k"
        .append(APPEND_S)
        .op(&[0x1a]) // iload_0 (receiver is append's return value)
        .append(APPEND_I)
        .op(&[0x57, 0x2b]) // pop; aload_1
        .sb_to_string()
        .op(&[0xb0]) // areturn
        .0;
    let chain = recognize(&code).expect("chained local builder does not escape");
    assert_eq!(chain.parts.len(), 2);
    assert_eq!(
        chain.local.as_ref().unwrap().append_result_pops,
        vec![false, true]
    );
    assert_eq!(chain.local.as_ref().unwrap().receiver_loads.len(), 3);
}

#[test]
fn a_local_builder_can_be_used_after_prior_stack_values_are_loaded() {
    let code = Asm::new()
        .sb_new()
        .op(&[0x4c]) // astore_1
        .op(&[0x2b, 0x12, 30]) // aload_1; ldc "k"
        .append(APPEND_S)
        .op(&[0x57, 0x20, 0x2b]) // pop; lload_2; aload_1
        .sb_to_string()
        .op(&[0x58, 0xb0]) // pop2; areturn
        .0;
    let chain = recognize(&code).expect("prior stack values survive local toString");
    assert_eq!(chain.parts.len(), 1);
    assert_eq!(chain.to_string_pc, 17);
}

#[test]
fn a_local_string_operand_is_not_mistaken_for_the_builder_receiver() {
    let code = Asm::new()
        .sb_new()
        .op(&[0x4c]) // astore_1
        .op(&[0x2b, 0x2c]) // aload_1; aload_2
        .append(APPEND_S)
        .op(&[0x57, 0x2b]) // pop; aload_1
        .sb_to_string()
        .op(&[0xb0]) // areturn
        .0;
    let chain = recognize(&code).expect("String local is an operand, not a receiver");
    assert_eq!(chain.parts.len(), 1);
    assert!(matches!(chain.parts[0], SbChainPart::Value { .. }));
}

#[test]
fn a_local_builder_used_after_to_string_is_refused() {
    let code = Asm::new()
        .sb_new()
        .op(&[0x4c]) // astore_1
        .op(&[0x2b, 0x12, 30]) // aload_1; ldc "k"
        .append(APPEND_S)
        .op(&[0x57, 0x2b]) // pop; aload_1
        .sb_to_string()
        .op(&[0x57, 0x2b]) // pop result; the builder escapes here
        .invoke(0xb6, SET_LENGTH)
        .0;
    assert!(recognize(&code).is_err());
}

#[test]
fn an_escaping_builder_is_refused() {
    // foo(new SB().append(i)): the builder is passed after an append.
    let passed = Asm::new()
        .sb_new()
        .op(&[0x1a])
        .append(APPEND_I)
        .invoke(0xb8, OTHER_CALL)
        .0;
    assert!(recognize(&passed).is_err());
    // Stored after an append (`b = new SB().append(i); ...`).
    let stored_late = Asm::new()
        .sb_new()
        .op(&[0x1a])
        .append(APPEND_I)
        .op(&[0x4c])
        .0;
    assert!(recognize(&stored_late).is_err());
    // Duplicated (compared, locked, or passed while kept).
    let dup = Asm::new()
        .sb_new()
        .op(&[0x59]) // dup
        .op(&[0xc2]) // monitorenter
        .0;
    assert!(recognize(&dup).is_err());
    // An operand that reads the builder from below its own values
    // (`swap` would put the builder on top).
    let swap = Asm::new()
        .sb_new()
        .op(&[0x1a, 0x5f]) // iload_0; swap
        .0;
    assert!(recognize(&swap).is_err());
}

#[test]
fn a_loop_carried_builder_is_refused() {
    // `new SB()` then a backward branch into the chain region.
    let code = Asm::new()
        .sb_new()
        .op(&[0x1a]) // pc 7: iload_0 (a loop head)
        .append(APPEND_I)
        .op(&[0xa7, 0xff, 0xfc]) // pc 11: goto 7
        .0;
    assert_eq!(
        recognize(&code).unwrap_err(),
        "a branch target inside the chain"
    );
    // A branch target on the toString of an otherwise fine chain.
    let mut code = Asm::new()
        .sb_new()
        .op(&[0x1a])
        .append(APPEND_I)
        .sb_to_string()
        .op(&[0xb0])
        .0;
    let ts = 11;
    code.extend_from_slice(&[0xa7]);
    let back = (ts as i16 - code.len() as i16 + 1).to_be_bytes();
    code.extend_from_slice(&back);
    assert_eq!(
        recognize(&code).unwrap_err(),
        "a branch target inside the chain"
    );
}

#[test]
fn an_object_append_and_other_builder_members_are_refused() {
    let object = Asm::new()
        .sb_new()
        .op(&[0x2b])
        .append(APPEND_O)
        .sb_to_string()
        .0;
    assert!(recognize(&object).unwrap_err().contains("append overload"));
    let set_length = Asm::new()
        .sb_new()
        .op(&[0x1a])
        .append(APPEND_I)
        .op(&[0x03])
        .invoke(0xb6, SET_LENGTH)
        .0;
    assert!(recognize(&set_length)
        .unwrap_err()
        .contains("other than append"));
    let insert = Asm::new()
        .sb_new()
        .op(&[0x03, 0x10, 0x41])
        .invoke(0xb6, INSERT)
        .sb_to_string()
        .0;
    assert!(recognize(&insert).is_err());
    // An operand that is a call is admitted with `has_call = true`.
    let call = Asm::new()
        .sb_new()
        .op(&[0x2b])
        .invoke(0xb6, OTHER_CALL)
        .append(APPEND_I)
        .sb_to_string()
        .0;
    assert!(recognize(&call).expect("call operand is admitted").has_call);
    // Division and iinc still have no safe re-execution snapshot path.
    for operand in [
        &[0x1a, 0x1b, 0x6c, 0x00], // iload_0; iload_1; idiv; nop
        &[0x84, 0, 1, 0x1a], // iinc 0 1; iload_0
    ] {
        let code = Asm::new()
            .sb_new()
            .op(operand)
            .append(APPEND_I)
            .sb_to_string()
            .0;
        assert!(recognize(&code).is_err(), "{operand:?}");
    }
}


#[test]
fn a_chain_across_a_try_boundary_is_refused() {
    let code = sb_chunk();
    // A range that starts inside the chain.
    assert!(recognize_at(&code, 0, &[(9, 40, 30)]).is_err());
    // A range that ends inside the chain.
    assert!(recognize_at(&code, 0, &[(0, 13, 30)]).is_err());
    // A handler inside the chain.
    assert!(recognize_at(&code, 0, &[(40, 50, 13)]).is_err());
}

#[test]
fn the_method_scan_returns_only_fusable_chains() {
    // A fusable chain, then a refused one (Object append).
    let mut code = sb_chunk();
    code.pop(); // drop the areturn
    code.push(0x57); // pop
    let second = code.len();
    let tail = Asm::new()
        .sb_new()
        .op(&[0x2b])
        .append(APPEND_O)
        .sb_to_string()
        .op(&[0xb0])
        .0;
    code.extend_from_slice(&tail);
    let targets = crate::bytecode_analysis::branch_target_map(&code, code.len());
    let src = SbChainCode {
        code: &code,
        branch_targets: &targets,
        handlers: &[],
    };
    let c = code.clone();
    let invoke_at = move |pc: usize| member(u16::from_be_bytes([c[pc + 1], c[pc + 2]]));
    let ldc_string_at = |_pc: usize| Some(units("k"));
    let field_at = |_cp: u16| None;
    let chains = fusable_sb_chains(&src, &invoke_at, &ldc_string_at, &field_at);
    assert_eq!(chains.len(), 1);
    assert_eq!(chains[0].new_pc, 0);
    assert!(chains.iter().all(|c| c.new_pc != second));
}

/// sbf21: the census lists the refused builder chains with the
/// recognizer's reasons and leaves fused chains and other classes' `new`
/// out.
#[test]
fn sbf21_the_census_lists_refused_chains_only() {
    // chain 1 (fusable), pop, chain 2 (Object append: refused),
    // pop, `new Object; dup; invokespecial Object.<init>` (not a builder).
    let mut code = sb_chunk();
    code.pop(); // drop the areturn
    code.push(0x57); // pop
    let second = code.len();
    let tail = Asm::new()
        .sb_new()
        .op(&[0x2b])
        .append(APPEND_O)
        .sb_to_string()
        .op(&[0x57])
        .op(&[0xbb, 0, 21, 0x59])
        .invoke(0xb7, OTHER_INIT)
        .op(&[0x57, 0xb1])
        .0;
    code.extend_from_slice(&tail);
    let targets = crate::bytecode_analysis::branch_target_map(&code, code.len());
    let src = SbChainCode {
        code: &code,
        branch_targets: &targets,
        handlers: &[],
    };
    let c = code.clone();
    let invoke_at = move |pc: usize| member(u16::from_be_bytes([c[pc + 1], c[pc + 2]]));
    let ldc_string_at = |_pc: usize| Some(units("k"));
    let field_at = |_cp: u16| None;
    let refused = refused_sb_chains(&src, &invoke_at, &ldc_string_at, &field_at);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0].0, second);
    assert!(refused[0].1.contains("append overload"));
}



/// sbf21 (SBFUSE21-3): `new StringBuilder(<constant>)` fuses for a
/// non-negative `int` constant (the capacity is unobservable) and nothing
/// else does: a negative one must keep throwing, a variable one may.
#[test]
fn sbf21_a_constant_capacity_constructor_is_fused() {
    let with_capacity = |arg: &[u8]| {
        let code = Asm::new()
            .op(&[0xbb, 0, 20, 0x59])
            .op(arg)
            .invoke(0xb7, SB_INIT_INT)
            .op(&[0x1a]) // iload_0
            .append(APPEND_I)
            .sb_to_string()
            .op(&[0xb0])
            .0;
        recognize(&code)
    };
    let chain = with_capacity(&[0x10, 32]).expect("bipush 32 is a capacity constant");
    assert_eq!(chain.init_pc(), 6);
    assert_eq!(chain.first_operand_pc(), 9);
    assert_eq!(chain.constructor_literal, None);
    assert_eq!(chain.parts.len(), 1);
    assert_eq!(chain.recipe().descriptor, "(I)Ljava/lang/String;");
    assert!(with_capacity(&[0x03]).is_ok(), "iconst_0");
    assert!(with_capacity(&[0x11, 0x7f, 0xff]).is_ok(), "sipush 32767");
    assert!(with_capacity(&[0x11, 0x80, 0x00]).is_err(), "sipush -32768");
    assert!(with_capacity(&[0x02]).is_err(), "iconst_m1 throws");
    assert!(with_capacity(&[0x10, 0xff]).is_err(), "bipush -1 throws");
    assert!(with_capacity(&[0x1b]).is_err(), "iload_1 is a variable capacity");
    assert!(with_capacity(&[0x09]).is_err(), "lconst_0 is no int");
    // The constant before the no-argument constructor is no capacity.
    let wrong = Asm::new()
        .op(&[0xbb, 0, 20, 0x59, 0x10, 32])
        .invoke(0xb7, SB_INIT)
        .op(&[0x1a])
        .append(APPEND_I)
        .sb_to_string()
        .0;
    assert!(recognize(&wrong).is_err());
}

/// sbf21: an append result left on the stack escapes. With the builder in
/// a local, `foo(sb.append(1), sb.toString())` and `foo(sb.append(1),
/// sb.append(2))` load the local a second time while the first append's
/// result (the builder itself) is on the stack; fusing either would drop
/// that live reference.
#[test]
fn sbf21_a_second_local_reference_while_an_append_result_is_live_is_refused() {
    let to_string_while_live = Asm::new()
        .sb_new()
        .op(&[0x4c, 0x2b, 0x04]) // astore_1; aload_1; iconst_1
        .append(APPEND_I)
        .op(&[0x2b]) // aload_1: a second reference, the first append result below
        .sb_to_string()
        .invoke(0xb8, OTHER_CALL)
        .op(&[0xb0])
        .0;
    assert!(recognize(&to_string_while_live)
        .unwrap_err()
        .contains("append result is on the stack"));
    let append_while_live = Asm::new()
        .sb_new()
        .op(&[0x4c, 0x2b, 0x04]) // astore_1; aload_1; iconst_1
        .append(APPEND_I)
        .op(&[0x2b, 0x05]) // aload_1; iconst_2
        .append(APPEND_I)
        .op(&[0x57, 0x2b]) // pop; aload_1
        .sb_to_string()
        .op(&[0xb0])
        .0;
    assert!(recognize(&append_while_live)
        .unwrap_err()
        .contains("append result is on the stack"));
}

/// sbf21: the fusion drops the chain's `astore`, so a read of the slot at
/// a lower pc that runs again through a back edge would see the old value.
#[test]
fn sbf21_a_local_builder_read_before_the_chain_in_a_loop_is_refused() {
    // 0: aload_1 (a loop head)   1: pop
    // 2: new / dup / <init> (7 bytes)   9: astore_1
    // 10: aload_1   11: iload_0   12: append(I)   15: pop
    // 16: aload_1   17: toString   20: pop   21: goto 0, or return
    let body = |backward: bool| {
        let tail: &[u8] = if backward {
            &[0xa7, 0xff, 0xeb] // goto -21
        } else {
            &[0xb1] // return
        };
        Asm::new()
            .op(&[0x2b, 0x57])
            .sb_new()
            .op(&[0x4c, 0x2b, 0x1a])
            .append(APPEND_I)
            .op(&[0x57, 0x2b])
            .sb_to_string()
            .op(&[0x57])
            .op(tail)
            .0
    };
    assert_eq!(
        recognize_at(&body(true), 2, &[]).unwrap_err(),
        "the local builder is read before the chain in a loop"
    );
    // Straight-line code that merely read the slot before is fine.
    let chain = recognize_at(&body(false), 2, &[]).expect("no loop, no stale read");
    assert_eq!(chain.parts.len(), 1);
    // A handler at or before the read that covers the chain reaches it too.
    let code = body(false);
    assert!(recognize_at(&code, 2, &[(2, code.len(), 0)]).is_err());
}

/// sbf21: `invoke_at` reads a pool index from the two bytes after ANY
/// opcode, so the `aload; toString` lookahead must see an `invokevirtual`.
#[test]
fn sbf21_the_to_string_lookahead_requires_an_invokevirtual() {
    // aload_1 followed by `sipush 6`, whose operand bytes read as the
    // pool index of StringBuilder.toString.
    let code = Asm::new()
        .sb_new()
        .op(&[0x4c, 0x2b, 0x11, 0x00, 0x06]) // astore_1; aload_1; sipush 6 (== TO_STRING)
        .0;
    assert!(recognize(&code).is_err());
}

