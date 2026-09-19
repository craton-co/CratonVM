// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Bytecode-verifier corpus: matched valid / invalid `.class` byte arrays.
//!
//! Every case below is built as real class-file bytes by [`builder`], parsed by
//! `cratonvm_reader::read_class`, wrapped in a `Class`, and handed to
//! `verifier::verify_class_bytecode` — the same Pass-3 entry point
//! `ClassManager::define_class_with_options` calls.
//!
//! **Each rejection has a valid counterpart.** A verifier that rejects
//! everything satisfies "no malformed class is accepted" trivially and is
//! useless; the paired tests are what make the negative results mean something.
//!
//! Coverage of these cases against the JVMS rules they exercise is tabulated in
//! `docs/security/verifier/coverage.md`.

mod builder;

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use builder::op;
use builder::{branch, same_frame, u16_op, ClassBuilder, Handler, MethodSpec};

use cratonvm_classloading::class_origin::ClassOrigin;
use cratonvm_classloading::verifier::verify_class_bytecode;
use cratonvm_classloading::vtype::ClassHierarchy;
use cratonvm_classloading::{Class, ClassId, ClassLoaderId, ClassState};
use cratonvm_reader::class_file::ClassFile;
use cratonvm_reader::force_decode_all;
use cratonvm_types::error::LinkageError;

const JAVA_5: u16 = 49;
const JAVA_8: u16 = 52;

/// Distinct `ClassId` per corpus case.
///
/// `verify_class_bytecode` publishes the per-class type maps into a
/// process-wide side table on success, and that install is first-writer-wins.
/// Reusing one id across cases would not change any verdict, but it would make
/// the side table describe whichever case happened to run first — so give each
/// case its own id and keep the table honest.
fn next_id() -> ClassId {
    static NEXT: AtomicU32 = AtomicU32::new(800_000);
    ClassId::new(NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A deliberately *narrow* hierarchy.
///
/// `is_interface` answers `false` for everything: JVMS §4.10.1.2 lets the
/// verifier treat any reference as assignable to an interface, so a hierarchy
/// that claimed everything was an interface would make the type-confusion cases
/// pass vacuously.
struct CorpusHierarchy;

impl ClassHierarchy for CorpusHierarchy {
    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        child == parent || parent == "java/lang/Object"
    }

    fn is_direct_superclass(&self, _child: &str, parent: &str) -> bool {
        parent == "java/lang/Object"
    }

    fn common_superclass(&self, a: &str, b: &str) -> String {
        if a == b {
            a.to_string()
        } else {
            "java/lang/Object".to_string()
        }
    }

    fn is_interface(&self, _name: &str) -> bool {
        false
    }
}

/// Read `bytes` and force every attribute to decode, surfacing the reader's
/// verdict rather than asserting on it.
///
/// The `Code` attribute decodes lazily, so a malformed exception table can
/// surface either from `read_class` (the fixed-size `start_pc`/`end_pc`/
/// `handler_pc` bounds, checked while the attribute body is walked) or from
/// `force_decode_all` (the `catch_type` constant-pool cross-check, which needs
/// the pool). Both are the same layer as far as the corpus is concerned.
fn read_and_decode(bytes: &[u8]) -> Result<ClassFile, cratonvm_reader::ClassReaderError> {
    let mut parsed = cratonvm_reader::read_class(bytes)?;
    for method in parsed.methods.iter_mut() {
        force_decode_all(&mut method.attributes, &parsed.constant_pool)?;
    }
    Ok(parsed)
}

/// Parse `bytes` and run Pass 3 over the result.
///
/// A parse failure is an assertion failure, not a rejection: the corpus is
/// about what the **verifier** does, so every case must be readable. Cases the
/// reader legitimately rejects (e.g. `code_length == 0`) are covered by the
/// unit tests in `classloading/src/verifier.rs` instead, and by
/// [`reject_at_parse`] below where the corpus still wants to pin the shape.
fn verify(bytes: &[u8]) -> Result<(), LinkageError> {
    let ClassFile {
        version,
        constant_pool,
        access_flags,
        this_class,
        methods,
        ..
    } = read_and_decode(bytes).expect("corpus class file must parse");

    let class = Class {
        id: next_id(),
        loader_id: ClassLoaderId::Application,
        name: this_class,
        source_file: None,
        version,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool,
        access_flags,
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods,
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        signature: None,
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: ClassOrigin::VmInternal,
        has_finalizer: false,
        code_source: None,
        array_info: None,
        init_state: Arc::new(AtomicU8::new(0)),
        record_object_methods: AtomicU8::new(0),
    };

    verify_class_bytecode(&class, &CorpusHierarchy)
}

/// Assert the class verifies, reporting the rejection when it does not.
fn accept(label: &str, bytes: &[u8]) {
    if let Err(e) = verify(bytes) {
        panic!("{label}: expected the verifier to ACCEPT this class, got: {e}");
    }
}

/// Assert the class is rejected, and that the message mentions `expect_msg`.
///
/// Matching on the message is what keeps a test honest: without it, a case can
/// silently start failing for an unrelated reason and still look green.
fn reject(label: &str, bytes: &[u8], expect_msg: &str) {
    match verify(bytes) {
        Ok(()) => panic!("{label}: expected the verifier to REJECT this class, it accepted"),
        Err(e) => {
            assert!(
                matches!(e, LinkageError::VerifyError { .. }),
                "{label}: rejection must be a VerifyError, got {e:?}"
            );
            let text = e.to_string();
            assert!(
                text.contains(expect_msg),
                "{label}: rejection message should mention {expect_msg:?}, got: {text}"
            );
        }
    }
}

/// Assert the class never reaches the verifier because the **reader** rejects
/// it, and that the message mentions `expect_msg`.
///
/// Some JVMS §4.7.3 exception-table constraints are format constraints, not
/// type-state ones: `cratonvm_reader` enforces them while it walks the `Code`
/// attribute, so bytes carrying those shapes are a `ClassReaderError` and the
/// verifier is never consulted. That is the correct layering — the malformed
/// class is refused strictly earlier than a `VerifyError` would refuse it — but
/// it means the corpus's [`reject`] helper cannot express these cases: it would
/// panic in [`verify`]'s `expect("must parse")` before making any assertion.
///
/// So pin them here instead, at the layer that actually decides. The verifier
/// keeps its own copy of each of these checks as defence in depth for producers
/// that build a `CodeAttribute` in memory rather than decoding it from bytes;
/// that half is exercised directly by `classloading/src/verifier.rs`'s
/// `structural_rejects_*` and `*_catch_type_*` unit tests.
fn reject_at_parse(label: &str, bytes: &[u8], expect_msg: &str) {
    match read_and_decode(bytes) {
        Ok(_) => panic!("{label}: expected the reader to REJECT these bytes, it parsed them"),
        Err(e) => {
            let text = e.to_string();
            assert!(
                text.contains(expect_msg),
                "{label}: rejection message should mention {expect_msg:?}, got: {text}"
            );
            assert!(
                text.contains("JVMS §4.7.3"),
                "{label}: rejection should cite the rule it enforces, got: {text}"
            );
        }
    }
}

/// One `static m()V` method in a fresh class.
fn one_method(name: &str, major: u16, spec: MethodSpec) -> Vec<u8> {
    let mut b = ClassBuilder::new(name, major);
    b.add_method(spec);
    b.build()
}

// ===========================================================================
// Operand stack bounds (JVMS §4.9.1: the stack never underflows nor exceeds
// max_stack)
// ===========================================================================

#[test]
fn stack_underflow_rejected() {
    // `pop` with nothing on the stack.
    let bytes = one_method(
        "corpus/Underflow",
        JAVA_8,
        MethodSpec::new("m", "()V", 1, 1, vec![op::POP, op::RETURN]),
    );
    reject("stack underflow", &bytes, "underflow");
}

#[test]
fn balanced_stack_accepted() {
    let bytes = one_method(
        "corpus/UnderflowOk",
        JAVA_8,
        MethodSpec::new("m", "()V", 1, 1, vec![op::ICONST_0, op::POP, op::RETURN]),
    );
    accept("balanced stack", &bytes);
}

#[test]
fn stack_overflow_rejected() {
    // Two pushes with `max_stack = 1`.
    let bytes = one_method(
        "corpus/Overflow",
        JAVA_8,
        MethodSpec::new(
            "m",
            "()V",
            1,
            1,
            vec![op::ICONST_0, op::ICONST_0, op::POP, op::POP, op::RETURN],
        ),
    );
    reject("stack overflow", &bytes, "overflow");
}

#[test]
fn stack_within_max_stack_accepted() {
    // The same body with an honest `max_stack`.
    let bytes = one_method(
        "corpus/OverflowOk",
        JAVA_8,
        MethodSpec::new(
            "m",
            "()V",
            2,
            1,
            vec![op::ICONST_0, op::ICONST_0, op::POP, op::POP, op::RETURN],
        ),
    );
    accept("stack within max_stack", &bytes);
}

// ===========================================================================
// Local variable bounds (JVMS §4.9.1)
// ===========================================================================

#[test]
fn local_index_past_max_locals_rejected() {
    // `istore_3` with `max_locals = 1`.
    let bytes = one_method(
        "corpus/BadLocal",
        JAVA_8,
        MethodSpec::new(
            "m",
            "()V",
            1,
            1,
            vec![op::ICONST_0, op::ISTORE_3, op::RETURN],
        ),
    );
    reject("local index past max_locals", &bytes, "max_locals");
}

#[test]
fn local_index_within_max_locals_accepted() {
    let bytes = one_method(
        "corpus/BadLocalOk",
        JAVA_8,
        MethodSpec::new(
            "m",
            "()V",
            1,
            4,
            vec![op::ICONST_0, op::ISTORE_3, op::RETURN],
        ),
    );
    accept("local index within max_locals", &bytes);
}

#[test]
fn max_locals_smaller_than_arguments_rejected() {
    // `static m(J)V` needs two local slots for its single `long` argument.
    let bytes = one_method(
        "corpus/ShortLocals",
        JAVA_8,
        MethodSpec::new("m", "(J)V", 1, 1, vec![op::RETURN]),
    );
    reject("max_locals under-declared", &bytes, "max_locals");
}

#[test]
fn max_locals_covering_arguments_accepted() {
    let bytes = one_method(
        "corpus/ShortLocalsOk",
        JAVA_8,
        MethodSpec::new("m", "(J)V", 1, 2, vec![op::RETURN]),
    );
    accept("max_locals covers arguments", &bytes);
}

// ===========================================================================
// Category-2 slot pairing (JVMS §4.10.1.6)
// ===========================================================================

/// `lconst_0; lstore_0; iconst_0; istore_<n>; lload_0; pop2; return`
///
/// With `n == 1` the `istore` lands on the long's upper half and splits it;
/// with `n == 2` the pair survives.
fn cat2_body(istore: u8) -> Vec<u8> {
    vec![
        op::LCONST_0,
        op::LSTORE_0,
        op::ICONST_0,
        istore,
        op::LLOAD_0,
        op::POP2,
        op::RETURN,
    ]
}

#[test]
fn split_category2_local_rejected() {
    let bytes = one_method(
        "corpus/SplitLong",
        JAVA_8,
        MethodSpec::new("m", "()V", 2, 3, cat2_body(op::ISTORE_1)),
    );
    reject("split category-2 local", &bytes, "lload");
}

#[test]
fn intact_category2_local_accepted() {
    let bytes = one_method(
        "corpus/SplitLongOk",
        JAVA_8,
        MethodSpec::new("m", "()V", 2, 3, cat2_body(op::ISTORE_2)),
    );
    accept("intact category-2 local", &bytes);
}

// ===========================================================================
// Branch targets (JVMS §4.9.1)
// ===========================================================================

#[test]
fn branch_target_outside_the_code_array_rejected() {
    // `goto +100` in a 4-byte method.
    let mut code = branch(op::GOTO, 100).to_vec();
    code.push(op::RETURN);
    let bytes = one_method(
        "corpus/WildGoto",
        JAVA_5,
        MethodSpec::new("m", "()V", 1, 1, code),
    );
    reject("branch target out of range", &bytes, "out of range");
}

#[test]
fn branch_into_the_middle_of_an_instruction_rejected() {
    // 0: goto +4 → offset 4, the second byte of the `sipush` at 3.
    let mut code = branch(op::GOTO, 4).to_vec();
    code.extend_from_slice(&u16_op(op::SIPUSH, 1));
    code.push(op::POP);
    code.push(op::RETURN);
    let bytes = one_method(
        "corpus/MidInsnGoto",
        JAVA_5,
        MethodSpec::new("m", "()V", 1, 1, code),
    );
    reject(
        "branch into an instruction",
        &bytes,
        "not an instruction boundary",
    );
}

#[test]
fn branch_to_an_instruction_boundary_accepted() {
    // The same shape with `goto +3`, which lands on the `sipush`.
    let mut code = branch(op::GOTO, 3).to_vec();
    code.extend_from_slice(&u16_op(op::SIPUSH, 1));
    code.push(op::POP);
    code.push(op::RETURN);
    let bytes = one_method(
        "corpus/MidInsnGotoOk",
        JAVA_5,
        MethodSpec::new("m", "()V", 1, 1, code),
    );
    accept("branch to an instruction boundary", &bytes);
}

// ===========================================================================
// Exception handlers (JVMS §4.7.3 / §4.9.1)
// ===========================================================================

/// ```text
/// 0: sipush 1     (guarded)
/// 3: pop          (guarded)
/// 4: return
/// 5: astore_1     ← handler entry: `[Throwable]` on a cleared stack
/// 6: return
/// ```
/// Instruction boundaries at 0, 3, 4, 5 and 6.
fn handler_body() -> Vec<u8> {
    let mut code = u16_op(op::SIPUSH, 1).to_vec();
    code.push(op::POP);
    code.push(op::RETURN);
    code.push(op::ASTORE_1);
    code.push(op::RETURN);
    code
}

/// The well-formed handler for [`handler_body`]: guards `0..4`, catches at 5.
fn good_handler() -> Handler {
    Handler::catch_all(0, 4, 5)
}

#[test]
fn handler_pc_inside_an_instruction_rejected() {
    // handler_pc = 1 is the second byte of the `sipush` at 0.
    let bytes = one_method(
        "corpus/BadHandler",
        JAVA_5,
        MethodSpec::new("m", "()V", 1, 2, handler_body())
            .with_handlers(vec![Handler::catch_all(0, 4, 1)]),
    );
    reject("handler_pc mid-instruction", &bytes, "instruction boundary");
}

// The next three cases were written as `reject(...)` — verifier-level
// rejections — and that was accurate when they were added: the matching check
// lives in `verify_method_structural` for the first two and in
// `bytecode_verifier::catch_type_of` for the third, and all three are still
// there. The reader has since grown the same three §4.7.3 constraints, and it
// runs first, so these bytes no longer reach Pass 3 at all.
//
// The shape stays covered and the rejection stays asserted; only the layer
// named by the assertion changed. See [`reject_at_parse`] for why they are not
// simply moved to a `VerifyError` expectation, and the `structural_rejects_*`
// and `*_catch_type_*` unit tests in `classloading/src/verifier.rs` for the
// verifier's own half.

#[test]
fn handler_pc_past_the_code_array_rejected() {
    let bytes = one_method(
        "corpus/BadHandler2",
        JAVA_5,
        MethodSpec::new("m", "()V", 1, 2, handler_body())
            .with_handlers(vec![Handler::catch_all(0, 4, 99)]),
    );
    // JVMS §4.7.3: `handler_pc` must be a valid index into the code array.
    reject_at_parse("handler_pc out of range", &bytes, "handler_pc 99");
}

#[test]
fn inverted_handler_range_rejected() {
    let bytes = one_method(
        "corpus/BadHandler3",
        JAVA_5,
        MethodSpec::new("m", "()V", 1, 2, handler_body())
            .with_handlers(vec![Handler::catch_all(4, 0, 5)]),
    );
    // JVMS §4.7.3: `start_pc` must be less than `end_pc`.
    reject_at_parse(
        "inverted handler range",
        &bytes,
        "start_pc 4 must be less than end_pc 0",
    );
}

#[test]
fn handler_with_a_non_class_catch_type_rejected() {
    // `catch_type` points at a Utf8 constant rather than a CONSTANT_Class.
    let mut b = ClassBuilder::new("corpus/BadCatchType", JAVA_5);
    let bogus = b.utf8("not a class");
    b.add_method(
        MethodSpec::new("m", "()V", 1, 2, handler_body()).with_handlers(vec![Handler {
            start_pc: 0,
            end_pc: 4,
            handler_pc: 5,
            catch_type: bogus,
        }]),
    );
    // JVMS §4.7.3: `catch_type` is either 0 or a CONSTANT_Class index.
    reject_at_parse("non-Class catch_type", &b.build(), "CONSTANT_Class");
}

#[test]
fn well_formed_handler_accepted() {
    let bytes = one_method(
        "corpus/GoodHandler",
        JAVA_5,
        MethodSpec::new("m", "()V", 1, 2, handler_body()).with_handlers(vec![good_handler()]),
    );
    accept("well-formed handler", &bytes);
}

// ===========================================================================
// Type-confused merge (JVMS §4.10.2 worklist inference)
// ===========================================================================

/// ```text
///  0: iconst_0
///  1: ifeq 8
///  4: aconst_null      (reference on the stack)
///  5: goto 9
///  8: <push>           (int on the stack when `push == ICONST_0`)
///  9: astore_1         (requires a reference)
/// 10: return
/// ```
///
/// At offset 9 the two predecessors merge. `null ⊔ int` has no common
/// supertype in the verification lattice, so the merged slot is `Top` and the
/// `astore_1` must be rejected. With `push == ACONST_NULL` both edges agree and
/// the method is well-typed.
fn merge_body(push: u8) -> Vec<u8> {
    let mut code = vec![op::ICONST_0];
    code.extend_from_slice(&branch(op::IFEQ, 7)); // 1 → 8
    code.push(op::ACONST_NULL); // 4
    code.extend_from_slice(&branch(op::GOTO, 4)); // 5 → 9
    code.push(push); // 8
    code.push(op::ASTORE_1); // 9
    code.push(op::RETURN); // 10
    code
}

#[test]
fn type_confused_merge_rejected() {
    let bytes = one_method(
        "corpus/BadMerge",
        JAVA_5,
        MethodSpec::new("m", "()V", 1, 2, merge_body(op::ICONST_0)),
    );
    reject("int/reference merge", &bytes, "astore");
}

#[test]
fn consistent_merge_accepted() {
    let bytes = one_method(
        "corpus/GoodMerge",
        JAVA_5,
        MethodSpec::new("m", "()V", 1, 2, merge_body(op::ACONST_NULL)),
    );
    accept("consistent merge", &bytes);
}

// ===========================================================================
// Object initialization (JVMS §4.10.1.9)
// ===========================================================================

#[test]
fn constructor_returning_uninitialized_this_rejected() {
    // `<init>()V { return; }` — no `super()` call, so the caller would receive
    // an object whose superclass constructor never ran.
    let mut b = ClassBuilder::new("corpus/EscapeInit", JAVA_8);
    b.add_method(MethodSpec::new("<init>", "()V", 1, 1, vec![op::RETURN]).instance());
    reject("uninitializedThis escape", &b.build(), "uninitialized");
}

#[test]
fn constructor_calling_super_accepted() {
    let mut b = ClassBuilder::new("corpus/EscapeInitOk", JAVA_8);
    let super_ctor = b.method_ref("java/lang/Object", "<init>", "()V");
    let mut code = vec![op::ALOAD_0];
    code.extend_from_slice(&u16_op(op::INVOKESPECIAL, super_ctor));
    code.push(op::RETURN);
    b.add_method(MethodSpec::new("<init>", "()V", 1, 1, code).instance());
    accept("constructor calling super", &b.build());
}

#[test]
fn init_called_twice_rejected() {
    // `new C; dup; dup; invokespecial C.<init>; invokespecial C.<init>` — the
    // first call replaces every `uninitialized(0)` in the frame with the
    // initialized type, so the second call has an already-initialized receiver.
    let mut b = ClassBuilder::new("corpus/DoubleInit", JAVA_8);
    let target = b.class("corpus/DoubleInit");
    let ctor = b.method_ref("corpus/DoubleInit", "<init>", "()V");
    let mut code = u16_op(op::NEW, target).to_vec();
    code.push(op::DUP);
    code.push(op::DUP);
    code.extend_from_slice(&u16_op(op::INVOKESPECIAL, ctor));
    code.extend_from_slice(&u16_op(op::INVOKESPECIAL, ctor));
    code.push(op::POP);
    code.push(op::RETURN);
    b.add_method(MethodSpec::new("m", "()V", 3, 1, code));
    reject("double <init>", &b.build(), "uninitialized object");
}

#[test]
fn init_called_once_accepted() {
    let mut b = ClassBuilder::new("corpus/DoubleInitOk", JAVA_8);
    let target = b.class("corpus/DoubleInitOk");
    let ctor = b.method_ref("corpus/DoubleInitOk", "<init>", "()V");
    let mut code = u16_op(op::NEW, target).to_vec();
    code.push(op::DUP);
    code.extend_from_slice(&u16_op(op::INVOKESPECIAL, ctor));
    code.push(op::POP);
    code.push(op::RETURN);
    b.add_method(MethodSpec::new("m", "()V", 2, 1, code));
    accept("single <init>", &b.build());
}

// ===========================================================================
// Constant-pool cross-checks (JVMS §4.9.1)
// ===========================================================================

#[test]
fn new_with_a_non_class_operand_rejected() {
    let mut b = ClassBuilder::new("corpus/BadNewTag", JAVA_8);
    let not_a_class = b.utf8("corpus/BadNewTag");
    let mut code = u16_op(op::NEW, not_a_class).to_vec();
    code.push(op::POP);
    code.push(op::RETURN);
    b.add_method(MethodSpec::new("m", "()V", 1, 1, code));
    reject("new with a Utf8 operand", &b.build(), "CONSTANT_Class");
}

#[test]
fn new_with_an_out_of_range_operand_rejected() {
    let mut b = ClassBuilder::new("corpus/BadNewIndex", JAVA_8);
    let mut code = u16_op(op::NEW, 4_000).to_vec();
    code.push(op::POP);
    code.push(op::RETURN);
    b.add_method(MethodSpec::new("m", "()V", 1, 1, code));
    reject(
        "new with an out-of-range index",
        &b.build(),
        "CONSTANT_Class",
    );
}

#[test]
fn new_with_a_class_operand_accepted() {
    let mut b = ClassBuilder::new("corpus/GoodNewTag", JAVA_8);
    let target = b.class("java/lang/Object");
    let mut code = u16_op(op::NEW, target).to_vec();
    code.push(op::POP);
    code.push(op::RETURN);
    b.add_method(MethodSpec::new("m", "()V", 1, 1, code));
    accept("new with a Class operand", &b.build());
}

#[test]
fn malformed_method_descriptor_rejected() {
    // `(Ljava/lang/String)V` — the object parameter has no `;` terminator.
    let mut b = ClassBuilder::new("corpus/BadDescriptor", JAVA_8);
    let callee = b.method_ref("java/lang/Object", "sink", "(Ljava/lang/String)V");
    let mut code = vec![op::ACONST_NULL];
    code.extend_from_slice(&u16_op(op::INVOKESTATIC, callee));
    code.push(op::RETURN);
    b.add_method(MethodSpec::new("m", "()V", 1, 1, code));
    reject("malformed descriptor", &b.build(), "method descriptor");
}

#[test]
fn well_formed_method_descriptor_accepted() {
    let mut b = ClassBuilder::new("corpus/GoodDescriptor", JAVA_8);
    let callee = b.method_ref("java/lang/Object", "sink", "(Ljava/lang/String;)V");
    let mut code = vec![op::ACONST_NULL];
    code.extend_from_slice(&u16_op(op::INVOKESTATIC, callee));
    code.push(op::RETURN);
    b.add_method(MethodSpec::new("m", "()V", 1, 1, code));
    accept("well-formed descriptor", &b.build());
}

#[test]
fn malformed_field_descriptor_rejected() {
    let mut b = ClassBuilder::new("corpus/BadFieldDescriptor", JAVA_8);
    let field = b.field_ref("corpus/BadFieldDescriptor", "f", "Ljava/lang/String");
    let mut code = u16_op(0xb2 /* getstatic */, field).to_vec();
    code.push(op::POP);
    code.push(op::RETURN);
    b.add_method(MethodSpec::new("m", "()V", 1, 1, code));
    reject("malformed field descriptor", &b.build(), "field descriptor");
}

#[test]
fn well_formed_field_descriptor_accepted() {
    let mut b = ClassBuilder::new("corpus/GoodFieldDescriptor", JAVA_8);
    let field = b.field_ref("corpus/GoodFieldDescriptor", "f", "Ljava/lang/String;");
    let mut code = u16_op(0xb2 /* getstatic */, field).to_vec();
    code.push(op::POP);
    code.push(op::RETURN);
    b.add_method(MethodSpec::new("m", "()V", 1, 1, code));
    accept("well-formed field descriptor", &b.build());
}

// ===========================================================================
// StackMapTable path (JVMS §4.10.1) — Java 7+ classes
// ===========================================================================

/// ```text
/// 0: iconst_0
/// 1: ifeq 4
/// 4: return
/// ```
/// A single `same_frame` at offset 4 covers the only branch target, which is
/// what strict verification requires of a Java 7+ class file.
fn framed_branch_body() -> Vec<u8> {
    let mut code = vec![op::ICONST_0];
    code.extend_from_slice(&branch(op::IFEQ, 3));
    code.push(op::RETURN);
    code
}

#[test]
fn java8_branch_with_a_declared_frame_accepted() {
    let bytes = one_method(
        "corpus/Framed",
        JAVA_8,
        MethodSpec::new("m", "()V", 1, 1, framed_branch_body()).with_stack_map(vec![same_frame(4)]),
    );
    accept("Java 8 branch with a declared frame", &bytes);
}

#[test]
fn java8_branch_without_a_stack_map_table_rejected() {
    // JVMS §4.10.1: a Java 7+ method with branches must ship a StackMapTable.
    let bytes = one_method(
        "corpus/Unframed",
        JAVA_8,
        MethodSpec::new("m", "()V", 1, 1, framed_branch_body()),
    );
    reject(
        "Java 8 branch with no StackMapTable",
        &bytes,
        "requires StackMapTable",
    );
}

#[test]
fn java8_branch_to_a_frameless_target_rejected() {
    // A StackMapTable that declares a frame somewhere OTHER than the branch
    // target. Strict verification (the default for non-bootstrap classes)
    // rejects the frameless target rather than typing it from the fall-through
    // edge.
    let mut code = vec![op::ICONST_0];
    code.extend_from_slice(&branch(op::IFEQ, 4)); // 1 → 5
    code.push(op::ICONST_0); // 4
    code.push(op::POP); // 5  ← branch target, no frame declared
    code.push(op::RETURN); // 6
    let bytes = one_method(
        "corpus/FramelessTarget",
        JAVA_8,
        MethodSpec::new("m", "()V", 1, 1, code).with_stack_map(vec![same_frame(4)]),
    );
    reject(
        "Java 8 branch to a frameless target",
        &bytes,
        "no StackMapTable frame",
    );
}

// ===========================================================================
// A method exercising several rules at once, as a smoke test that the
// verifier is not simply rejecting everything.
// ===========================================================================

#[test]
fn multi_rule_valid_class_accepted() {
    let mut b = ClassBuilder::new("corpus/Composite", JAVA_5);
    let super_ctor = b.method_ref("java/lang/Object", "<init>", "()V");
    let target = b.class("java/lang/Object");

    // `<init>()V { super(); }`
    let mut ctor = vec![op::ALOAD_0];
    ctor.extend_from_slice(&u16_op(op::INVOKESPECIAL, super_ctor));
    ctor.push(op::RETURN);
    b.add_method(MethodSpec::new("<init>", "()V", 1, 1, ctor).instance());

    // `static m()V` — a long held in a local pair, a guarded region whose
    // handler entry is reached only along the exception edge, and a branch.
    //
    //   0: lconst_0
    //   1: lstore_0     (guarded region 0..5)
    //   2: goto 5
    //   5: lload_0
    //   6: pop2
    //   7: return
    //   8: astore_2     ← handler entry
    //   9: return
    let mut m = vec![op::LCONST_0, op::LSTORE_0];
    m.extend_from_slice(&branch(op::GOTO, 3)); // 2 → 5
    m.push(op::LLOAD_0); // 5
    m.push(op::POP2); // 6
    m.push(op::RETURN); // 7
    m.push(op::ASTORE_2); // 8
    m.push(op::RETURN); // 9
    b.add_method(
        MethodSpec::new("m", "()V", 2, 3, m).with_handlers(vec![Handler::catch_all(0, 5, 8)]),
    );

    // `static n()V { new Object(); }` — but only the allocation, popped.
    let mut n = u16_op(op::NEW, target).to_vec();
    n.push(op::DUP);
    n.extend_from_slice(&u16_op(op::INVOKESPECIAL, super_ctor));
    n.push(op::POP);
    n.push(op::RETURN);
    b.add_method(MethodSpec::new("n", "()V", 2, 1, n));

    accept("composite valid class", &b.build());
}
