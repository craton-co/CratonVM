//! JIT round 21 wave 10, lane finalize: the proof that a constructor's
//! throw preceded every `<init>` call on its receiver
//! (`ctor_throw_precedes_initialization`). Pool entry 5 is another
//! class's `<init>` (`new IAE` before `super()`); every other entry may
//! initialize the receiver.
use super::ctor_throw_precedes_initialization as proven;
use super::*;

fn may(i: u16) -> bool {
    i != 5
}

/// The constructor class the unwinder tests run on (wave 15). The test
/// VM boots the synthetic JDK (`EMBEDDED_DEFAULT_JDK_MODE`), whose
/// `java/lang/StringBuilder` has no `()V` bytecode, so the w10-w14 tests
/// that borrowed `StringBuilder()` never reached their asserts.
const FIN_CLASS: &str = "cratonvm/test/R21FinCtor";
const FIN_BASE: &str = "cratonvm/test/R21FinCtorBase";

/// Defines `FIN_BASE` and, under it, the finalizable `FIN_CLASS` whose
/// `<init>()V` is `aload_0; bipush 16; invokespecial #6 (FIN_BASE.<init>(I)V);
/// return` (the shape `StringBuilder()` has on a real JDK), into the test
/// VM's class store. Returns the class id, the constructor's code and the
/// instance field count.
fn define_finalizable_ctor_class(shared: &SharedVm) -> (ClassId, Vec<u8>, usize) {
    use crate::classloading::{Class, ClassLoaderId, ClassState};
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::method::ClassFileMethod;
    const CODE: [u8; 7] = [0x2a, 0x10, 0x10, 0xb7, 0x00, 0x06, 0xb1];
    let class = |id: ClassId,
                 name: &str,
                 superclass: Option<ClassId>,
                 pool: Vec<ConstantPoolEntry>,
                 methods: Vec<ClassFileMethod>,
                 has_finalizer: bool| Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Initialized,
        initializing_thread: None,
        constant_pool: ConstantPool::new(pool),
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass,
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
        origin: cratonvm_classloading::ClassOrigin::default(),
        has_finalizer,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    };
    let ctor = ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: cratonvm_types::intern_arc("<init>"),
        descriptor: cratonvm_types::intern_arc("()V"),
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(CodeAttribute {
            max_stack: 2,
            max_locals: 1,
            code: cratonvm_reader::ByteView::from_vec(CODE.to_vec()),
            exception_table: vec![],
            attributes: vec![],
        })))],
    };
    let mut cm = shared.classes.class_manager.write();
    let base = cm.class_store.next_id();
    cm.class_store.add(class(
        base,
        FIN_BASE,
        None,
        vec![ConstantPoolEntry::Tombstone],
        vec![],
        false,
    ));
    let id = cm.class_store.next_id();
    cm.class_store.add(class(
        id,
        FIN_CLASS,
        Some(base),
        vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8(FIN_BASE.into()),
            ConstantPoolEntry::ClassReference { name_index: 1 },
            ConstantPoolEntry::Utf8("<init>".into()),
            ConstantPoolEntry::Utf8("(I)V".into()),
            ConstantPoolEntry::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            },
            ConstantPoolEntry::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            },
        ],
        vec![ctor],
        true,
    ));
    (id, CODE.to_vec(), 0)
}

/// The unwinder end to end: `FIN_CLASS()` (`aload_0; bipush 16;
/// invokespecial FIN_BASE.<init>(I)V; return`, a finalizable class built
/// in this VM's store, [`define_finalizable_ctor_class`]) unwound under an interpreted
/// creator frame. A throw at the `bipush` withdraws the registration at
/// the creator; one at the `super(16)` call keeps it.
#[test]
fn the_unwinder_withdraws_a_constructor_that_threw_before_super() {
    use crate::threading::jvm_thread::ThreadId;
    if !finalizer_register_at_object_init_enabled(&crate::config::VmConfig::default()) {
        return; // `CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT=0`: the defect.
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    // Wave 15: a hand-built finalizable class (the synthetic JDK's
    // `StringBuilder` has no `()V` bytecode).
    let (sb, code, num_fields) = define_finalizable_ctor_class(&shared);
    if code.len() != 7 || code[0] != 0x2a || code[1] != 0x10 || code[3] != 0xb7 {
        return; // Another JDK's body: the shape this test names is gone.
    }
    let exc_class = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/R21FinalizeThrown", 0)
        .expect("Compatible mode fabricates");
    let registered_after_throw_at = |throw_pc: usize| -> bool {
        let obj = shared.mem.heap.alloc_object(sb, num_fields);
        let addr = obj.as_ptr() as usize;
        shared.register_finalizable(addr);
        let exc = shared.mem.heap.alloc_object(exc_class, 0);
        let mut t = JvmThread::new(ThreadId(0x0F1A), "r21-finalize-unwind");
        // The creator: `invokespecial #1; return`, at its invoke.
        t.frames.push(Frame::new(
            ClassId::new(0x00F1_0C01),
            "cratonvm/test/R21Creator".to_string(),
            "make".to_string(),
            "()V".to_string(),
            None,
            vec![0xb7, 0, 1, 0xb1],
            Vec::new(),
            4,
            2,
            &[],
        ));
        t.frames[0].last_instr_pc = 0;
        t.frames.push(Frame::new(
            sb,
            FIN_CLASS.to_string(),
            "<init>".to_string(),
            "()V".to_string(),
            None,
            code.clone(),
            Vec::new(),
            4,
            2,
            &[Value::Object(Some(obj))],
        ));
        // Wave 13: each step of the chain on its own, so a failure names
        // the step (the w10 build failed this test without saying which).
        assert!(
            shared.mem.ref_processor.lock().is_registered_finalizable(addr),
            "registered before the throw"
        );
        assert!(
            super::super::class_has_finalizer_shared(&shared, obj),
            "the receiver reads finalizable"
        );
        assert_eq!(
            unconstructed_mark_of(&shared, &t, 1, throw_pc).is_some(),
            throw_pc == 1,
            "the proof at pc {throw_pc}"
        );
        let mut idx = 1usize;
        assert!(unwind_to_handler(&shared, &mut t, &mut idx, 0, exc, throw_pc).is_err());
        shared.mem.ref_processor.lock().is_registered_finalizable(addr)
    };
    assert!(!registered_after_throw_at(1), "thrown before super(16): withdrawn");
    assert!(registered_after_throw_at(3), "thrown at the super(16) call: kept");
}

/// t1 wave 9, lane I24V (`CRATONVM_FINALIZER_WITHDRAW_AT_CTOR_PUSH_FAILURE`):
/// the frame push of a constructor fails (SOE at the `invokespecial`).
/// At the `super(16)` call of the receiver's own constructor (the shape
/// the unwinder keeps above) and at the creator's `new F` call, the
/// registration goes; a row already marked constructed stays.
#[test]
fn a_failed_constructor_frame_push_withdraws_the_receiver() {
    use crate::threading::jvm_thread::ThreadId;
    let config = crate::config::VmConfig::default();
    if !finalizer_register_at_object_init_enabled(&config)
    {
        return; // A kill switch is set: the defect.
    }
    let reregister = config.finalizer_reregister_at_init_return;
    let shared = Arc::new(SharedVm::new(config));
    let (sb, code, num_fields) = define_finalizable_ctor_class(&shared);
    if code.len() != 7 || code[0] != 0x2a || code[1] != 0x10 || code[3] != 0xb7 {
        return; // Another JDK's body: the shape this test names is gone.
    }
    let creator = || {
        let mut f = Frame::new(
            ClassId::new(0x00F1_0C02),
            "cratonvm/test/I24vCreator".to_string(),
            "make".to_string(),
            "()V".to_string(),
            None,
            vec![0xb7, 0, 1, 0xb1],
            Vec::new(),
            4,
            2,
            &[],
        );
        f.last_instr_pc = 0;
        f
    };
    // `in_ctor`: the push failed at the constructor's `super(16)` (pc 3);
    // otherwise at the creator's call. `constructed`: an `<init>` of the
    // object returned before.
    let registered_after = |in_ctor: bool, constructed: bool| -> bool {
        let obj = shared.mem.heap.alloc_object(sb, num_fields);
        let addr = obj.as_ptr() as usize;
        shared.register_finalizable(addr);
        if constructed {
            shared.mem.ref_processor.lock().note_finalizable_init_returned(addr);
        }
        let mut t = JvmThread::new(ThreadId(0x0F1B), "i24v-ctor-push-failure");
        t.frames.push(creator());
        if in_ctor {
            t.frames.push(Frame::new(
                sb,
                FIN_CLASS.to_string(),
                "<init>".to_string(),
                "()V".to_string(),
                None,
                code.clone(),
                Vec::new(),
                4,
                2,
                &[Value::Object(Some(obj))],
            ));
            t.frames[1].last_instr_pc = 3;
            assert_eq!(ctor_push_failure_chain_bottom(&t, 1, addr), Some(1));
        } else {
            assert_eq!(ctor_push_failure_chain_bottom(&t, 0, addr), None);
        }
        withdraw_at_ctor_push_failure(&shared, &t, t.frames.len() - 1, obj);
        shared.mem.ref_processor.lock().is_registered_finalizable(addr)
    };
    assert!(!registered_after(true, false), "push failed at super(16): withdrawn");
    assert!(!registered_after(false, false), "push failed at the creator's call: withdrawn");
    if reregister {
        assert!(registered_after(true, true), "a constructed row stays");
    }
}

/// t1 wave 11, lane INTA (`CRATONVM_FINALIZER_WITHDRAW_AT_CTOR_ENTRY_PUSH_FAILURE`):
/// the interpreter ENTRY of a constructor (`execute`) cannot push its
/// frame. With no frame below (a compiled or reflective caller), below an
/// uncovered interpreted creator, and below the receiver's own
/// constructor at `super(16)`, the registration goes. A creator whose
/// handler covers the call and whose local holds the object (a retry)
/// only notes it, and a row already marked constructed stays.
#[test]
fn a_failed_constructor_entry_push_withdraws_the_receiver() {
    use crate::threading::jvm_thread::ThreadId;
    let config = crate::config::VmConfig::default();
    if !finalizer_register_at_object_init_enabled(&config)
    {
        return; // A kill switch is set: the old answer.
    }
    let reregister = config.finalizer_reregister_at_init_return;
    let shared = Arc::new(SharedVm::new(config));
    let (sb, code, num_fields) = define_finalizable_ctor_class(&shared);
    if code.len() != 7 || code[0] != 0x2a || code[1] != 0x10 || code[3] != 0xb7 {
        return; // Another JDK's body: the shape this test names is gone.
    }
    #[derive(Clone, Copy, PartialEq)]
    enum Below {
        Nothing,
        Creator,
        CoveredCreatorHoldingIt,
        OwnCtorAtSuper,
    }
    let registered_after = |below: Below, constructed: bool| -> bool {
        let obj = shared.mem.heap.alloc_object(sb, num_fields);
        let addr = obj.as_ptr() as usize;
        shared.register_finalizable(addr);
        if constructed {
            shared.mem.ref_processor.lock().note_finalizable_init_returned(addr);
        }
        let mut t = JvmThread::new(ThreadId(0x0F1C), "inta-ctor-entry-push-failure");
        if below != Below::Nothing {
            let covered = below == Below::CoveredCreatorHoldingIt;
            let table = if covered {
                vec![cratonvm_reader::attribute::ExceptionTableEntry {
                    start_pc: 0,
                    end_pc: 3,
                    handler_pc: 3,
                    catch_type: 0,
                }]
            } else {
                Vec::new()
            };
            let held = [Value::Object(Some(obj))];
            let mut f = Frame::new(
                ClassId::new(0x00F1_0C03),
                "cratonvm/test/IntaCreator".to_string(),
                "make".to_string(),
                "()V".to_string(),
                None,
                vec![0xb7, 0, 1, 0xb1],
                table,
                4,
                2,
                if covered { &held[..] } else { &[] },
            );
            f.last_instr_pc = 0;
            t.frames.push(f);
        }
        if below == Below::OwnCtorAtSuper {
            t.frames.push(Frame::new(
                sb,
                FIN_CLASS.to_string(),
                "<init>".to_string(),
                "()V".to_string(),
                None,
                code.clone(),
                Vec::new(),
                4,
                2,
                &[Value::Object(Some(obj))],
            ));
            t.frames[1].last_instr_pc = 3;
            assert_eq!(
                ctor_entry_push_failure_chain_bottom(&t, addr, t.frames.len()),
                1
            );
        } else {
            assert_eq!(
                ctor_entry_push_failure_chain_bottom(&t, addr, t.frames.len()),
                t.frames.len()
            );
        }
        withdraw_at_ctor_entry_push_failure(&shared, &t, obj);
        shared
            .mem
            .ref_processor
            .lock()
            .is_registered_finalizable(addr)
    };
    assert!(
        !registered_after(Below::Nothing, false),
        "no frame below: withdrawn"
    );
    assert!(
        !registered_after(Below::Creator, false),
        "below a creator: withdrawn"
    );
    assert!(
        !registered_after(Below::OwnCtorAtSuper, false),
        "below super(16): withdrawn"
    );
    assert!(
        registered_after(Below::CoveredCreatorHoldingIt, false),
        "a creator that could retry: noted, kept"
    );
    if reregister {
        assert!(registered_after(Below::Nothing, true), "a constructed row stays");
    }
}

#[test]
fn a_compiled_constructor_entry_failure_withdraws_only_an_unconstructed_receiver() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let (class_id, _, fields) = define_finalizable_ctor_class(&shared);
    let object = shared.mem.heap.alloc_object(class_id, fields);
    let addr = object.as_ptr() as usize;
    shared.register_finalizable(addr);
    withdraw_compiled_constructor_at_entry(&shared, 0);
    assert!(shared
        .mem
        .ref_processor
        .lock()
        .is_registered_finalizable(addr));
    withdraw_compiled_constructor_at_entry(&shared, addr as i64);
    assert!(!shared
        .mem
        .ref_processor
        .lock()
        .is_registered_finalizable(addr));
    // A retry can register the same address, but a fully constructed row
    // must survive a subsequent failed JNI re-initialization.
    shared.register_finalizable(addr);
    shared
        .mem
        .ref_processor
        .lock()
        .note_finalizable_init_returned(addr);
    withdraw_compiled_constructor_at_entry(&shared, addr as i64);
    assert!(shared
        .mem
        .ref_processor
        .lock()
        .is_registered_finalizable(addr));
}

#[test]
fn a_pushed_but_unrun_constructor_preserves_a_covering_creators_retry() {
    use crate::threading::jvm_thread::ThreadId;
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let (class_id, code, fields) = define_finalizable_ctor_class(&shared);
    for covering in [false, true] {
        let object = shared.mem.heap.alloc_object(class_id, fields);
        let addr = object.as_ptr() as usize;
        shared.register_finalizable(addr);
        let mut thread = JvmThread::new(ThreadId(0x0F1F), "unrun-constructor-entry");
        let table = if covering {
            vec![cratonvm_reader::attribute::ExceptionTableEntry {
                start_pc: 0,
                end_pc: 3,
                handler_pc: 3,
                catch_type: 0,
            }]
        } else {
            vec![]
        };
        thread.frames.push(Frame::new(
            class_id,
            FIN_CLASS.to_string(),
            "make".to_string(),
            "()V".to_string(),
            None,
            vec![0xb7, 0, 1, 0xb1],
            table,
            4,
            2,
            &[Value::Object(Some(object))],
        ));
        thread.frames.push(Frame::new(
            class_id,
            FIN_CLASS.to_string(),
            "<init>".to_string(),
            "()V".to_string(),
            None,
            code.clone(),
            vec![],
            4,
            2,
            &[Value::Object(Some(object))],
        ));
        withdraw_at_pushed_ctor_entry_failure(&shared, &thread, 1);
        assert_eq!(
            shared
                .mem
                .ref_processor
                .lock()
                .is_registered_finalizable(addr),
            covering
        );
    }
}

// `F(int x) { this(check(x)); }`: aload_0; iload_1; invokestatic #2;
// invokespecial #3; return.
const THIS_OF_CHECK: &[u8] = &[0x2a, 0x1b, 0xb8, 0, 2, 0xb7, 0, 3, 0xb1];

#[test]
fn a_throw_inside_the_argument_of_this_precedes_initialization() {
    assert!(proven(THIS_OF_CHECK, 2, &[], &may));
    // At the `this(..)` call itself: the callee may have initialized it.
    assert!(!proven(THIS_OF_CHECK, 5, &[], &may));
    // Not an instruction boundary (an operand byte of the invokestatic).
    assert!(!proven(THIS_OF_CHECK, 3, &[], &may));
    // A handler that starts before the call could bring control back.
    assert!(!proven(THIS_OF_CHECK, 2, &[0], &may));
    assert!(proven(THIS_OF_CHECK, 2, &[8], &may));
}

#[test]
fn a_throw_after_super_is_not_proven() {
    // aload_0; invokespecial #1; iload_1; invokestatic #2; return.
    let code = [0x2a, 0xb7, 0, 1, 0x1b, 0xb8, 0, 2, 0xb1];
    assert!(!proven(&code, 5, &[], &may));
}

#[test]
fn a_new_exception_before_super_is_not_an_initialization() {
    // iload_1; ifge +11; new #4; dup; invokespecial #5 (IAE.<init>);
    // athrow; aload_0; invokespecial #1; return.
    let code = [
        0x1b, 0x9c, 0, 11, 0xbb, 0, 4, 0x59, 0xb7, 0, 5, 0xbf, 0x2a, 0xb7, 0, 1, 0xb1,
    ];
    assert!(proven(&code, 11, &[], &may));
    assert!(proven(&code, 8, &[], &may));
    // Were entry 5 a possible `this(..)`, the athrow would follow it.
    assert!(!proven(&code, 11, &[], &|_| true));
}

#[test]
fn a_back_edge_over_the_initialization_refuses() {
    // iload_1; invokestatic #2; aload_0; invokespecial #1; goto -8.
    let code = [0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xa7, 0xff, 0xf8];
    assert!(!proven(&code, 1, &[], &may));
    // The same body with the goto replaced by `return` is proven.
    let code = [0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xb1];
    assert!(proven(&code, 1, &[], &may));
}

#[test]
fn a_write_of_local_0_or_a_subroutine_refuses() {
    // aconst_null; astore_0; ... (local 0 is no longer the receiver).
    let code = [0x01, 0x4b, 0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xb1];
    assert!(!proven(&code, 3, &[], &may));
    // wide astore 0.
    let code = [0x01, 0xc4, 0x3a, 0, 0, 0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xb1];
    assert!(!proven(&code, 6, &[], &may));
    // astore_1 is fine.
    let code = [0x01, 0x4c, 0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xb1];
    assert!(proven(&code, 3, &[], &may));
    // jsr anywhere.
    let code = [0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xb1, 0xa8, 0, 0];
    assert!(!proven(&code, 1, &[], &may));
    // A reserved opcode.
    let code = [0x1b, 0xca, 0x2a, 0xb7, 0, 1, 0xb1];
    assert!(!proven(&code, 0, &[], &may));
}

#[test]
fn a_lookupswitch_before_the_call_decodes() {
    // iload_1; lookupswitch (pad 2) default +19, 1 pair (5 -> +19);
    // aload_0; iload_1; invokestatic #2; invokespecial #3; return.
    let code = [
        0x1b, 0xab, 0, 0, 0, 0, 0, 19, 0, 0, 0, 1, 0, 0, 0, 5, 0, 0, 0, 19, 0x2a, 0x1b, 0xb8,
        0, 2, 0xb7, 0, 3, 0xb1,
    ];
    assert!(proven(&code, 22, &[], &may));
    // A truncated body refuses.
    assert!(!proven(&code[..24], 22, &[], &may));
}

/// Wave 11 (review item 1): a hidden class is stored `p/H/0x<hex>` and
/// its pool names `p/H`; its `this(..)` initializes.
#[test]
fn a_hidden_class_this_call_is_an_initialization() {
    use super::ctor_target_may_initialize as may_init;
    let sup = Some(("java/lang/Object", false));
    assert!(may_init("p/H/0x1f2e", true, sup, "p/H"), "pre-mangle name");
    assert!(may_init("p/H/0x1f2e", true, sup, "p/H/0x1f2e"));
    assert!(may_init("p/H", false, sup, "p/H"));
    assert!(may_init("p/H", false, sup, "java/lang/Object"), "super(..)");
    assert!(!may_init("p/H", false, sup, "java/lang/RuntimeException"));
    // Only a hidden class's name is read as mangled.
    assert!(!may_init("p/H/0x1f2e", false, sup, "p/H"));
    // A hidden superclass is spelled by its pre-mangle name too.
    assert!(may_init("p/K", false, Some(("p/S/0x77", true)), "p/S"));

    // `H(int x) { this(); throw new RuntimeException(); }`: aload_0;
    // invokespecial #1 (p/H.<init>()V); new #4; dup; invokespecial #5
    // (RuntimeException.<init>); athrow. The pool's #1 names `p/H`.
    let code = [0x2a, 0xb7, 0, 1, 0xbb, 0, 4, 0x59, 0xb7, 0, 5, 0xbf];
    let class_of = |i: u16| if i == 1 { "p/H" } else { "java/lang/RuntimeException" };
    let hidden_aware = |i: u16| may_init("p/H/0x1f2e", true, sup, class_of(i));
    assert!(
        !proven(&code, 11, &[], &hidden_aware),
        "the athrow follows this(): Object.<init> returned"
    );
    // The wave-10 compare (plain names) wrongly proved it.
    let plain = |i: u16| class_of(i) == "p/H/0x1f2e" || class_of(i) == "java/lang/Object";
    assert!(proven(&code, 11, &[], &plain));
}

/// Wave 11: the constructors kept interpreted are exactly those with a
/// throwing instruction where the proof would hold.
#[test]
fn only_a_constructor_that_can_throw_before_init_stays_interpreted() {
    use super::ctor_may_throw_before_initialization as may_throw;
    // `this(check(x))`: the invokestatic precedes the call.
    assert!(may_throw(THIS_OF_CHECK, &[], &may));
    // `super(); f(x);`: nothing before the call can throw.
    let code = [0x2a, 0xb7, 0, 1, 0x1b, 0xb8, 0, 2, 0xb1];
    assert!(!may_throw(&code, &[], &may));
    // `super(x, y)`: loads only.
    let code = [0x2a, 0x1b, 0x1c, 0xb7, 0, 1, 0xb1];
    assert!(!may_throw(&code, &[], &may));
    // `super(x / y)`: idiv throws.
    let code = [0x2a, 0x1b, 0x1c, 0x6c, 0xb7, 0, 1, 0xb1];
    assert!(may_throw(&code, &[], &may));
    // `super(x * y)`: imul does not.
    let code = [0x2a, 0x1b, 0x1c, 0x68, 0xb7, 0, 1, 0xb1];
    assert!(!may_throw(&code, &[], &may));
    // A body the proof refuses (a back edge over the call) gains nothing
    // from the interpreter.
    let code = [0x1b, 0xb8, 0, 2, 0x2a, 0xb7, 0, 1, 0xa7, 0xff, 0xf8];
    assert!(!may_throw(&code, &[], &may));
    // A handler before the call refuses too.
    assert!(!may_throw(THIS_OF_CHECK, &[0], &may));
}

/// Wave 11: the walk withdraws at its ENTRY frame (the creator is a
/// compiled method, a JIT helper or a native door), unless the
/// interpreted frame below stands at an `invokespecial` a handler covers
/// (review item 4: a retry of `<init>` could follow); then it only notes.
#[test]
fn the_entry_frame_withdraws_unless_the_frame_below_may_retry() {
    use crate::threading::jvm_thread::ThreadId;
    use cratonvm_reader::attribute::ExceptionTableEntry;
    if !finalizer_register_at_object_init_enabled(&crate::config::VmConfig::default()) {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    // Wave 15: a hand-built finalizable class (the synthetic JDK's
    // `StringBuilder` has no `()V` bytecode).
    let (sb, code, num_fields) = define_finalizable_ctor_class(&shared);
    if code.len() != 7 || code[0] != 0x2a || code[1] != 0x10 || code[3] != 0xb7 {
        return;
    }
    let exc_class = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/R21FinalizeThrownEntry", 0)
        .expect("Compatible mode fabricates");
    // `below`: the interpreted frame under the entry, with or without a
    // row covering its `invokespecial` at pc 0, and (wave 14) with or
    // without a local holding the object (the retry shape).
    let registered_after = |below: Option<(bool, bool)>| -> bool {
        let obj = shared.mem.heap.alloc_object(sb, num_fields);
        let addr = obj.as_ptr() as usize;
        shared.register_finalizable(addr);
        let exc = shared.mem.heap.alloc_object(exc_class, 0);
        let mut t = JvmThread::new(ThreadId(0x0F1B), "r21-finalize-entry");
        if let Some((covered, holds)) = below {
            let held = [Value::Object(Some(obj))];
            let table = if covered {
                vec![ExceptionTableEntry {
                    start_pc: 0,
                    end_pc: 3,
                    handler_pc: 4,
                    catch_type: 0,
                }]
            } else {
                Vec::new()
            };
            t.frames.push(Frame::new(
                ClassId::new(0x00F1_0C02),
                "cratonvm/test/R21OuterCreator".to_string(),
                "make".to_string(),
                "()V".to_string(),
                None,
                vec![0xb7, 0, 1, 0xb1, 0xbf],
                table,
                4,
                2,
                if holds { &held[..] } else { &[] },
            ));
            t.frames[0].last_instr_pc = 0;
        }
        let entry = t.frames.len();
        t.frames.push(Frame::new(
            sb,
            FIN_CLASS.to_string(),
            "<init>".to_string(),
            "()V".to_string(),
            None,
            code.clone(),
            Vec::new(),
            4,
            2,
            &[Value::Object(Some(obj))],
        ));
        let mut idx = entry;
        // The walk's entry is the constructor frame: `initial_frame_idx == entry`.
        assert!(unwind_to_handler(&shared, &mut t, &mut idx, entry, exc, 1).is_err());
        shared.mem.ref_processor.lock().is_registered_finalizable(addr)
    };
    assert!(!registered_after(None), "no frame below: withdrawn at the entry");
    assert!(!registered_after(Some((false, false))), "an uncovered creator below: withdrawn");
    assert!(!registered_after(Some((false, true))), "uncovered, a local holding it: withdrawn");
    assert!(
        registered_after(Some((true, true))),
        "a covered invokespecial and a local holding the object: kept (noted)"
    );
    assert!(
        !registered_after(Some((true, false))),
        "wave 14: covered, but no handler can reach the object: withdrawn"
    );
}

/// Wave 16 (lane finalize-reinit, residual 4R,
/// `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`). Case (a): the first
/// `FIN_CLASS()` attempt throws at its `bipush` under a creator outside
/// the walk (a compiled method) and is withdrawn at the entry; the
/// creator's retry returns normally from `FIN_CLASS()` on the same object,
/// which registers it again, exactly once. A `return`-pc the prefix scan
/// puts before `super(16)` registers nothing. Case (b): an object whose
/// `<init>` returned keeps its registration when a later `<init>` on it
/// throws before `super(..)`.
#[test]
fn a_later_init_return_registers_a_withdrawn_object_again_once() {
    use crate::threading::jvm_thread::ThreadId;
    if !finalizer_register_at_object_init_enabled(&crate::config::VmConfig::default()) {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    if !shared.mem.ref_processor.lock().reregister_at_init_return() {
        return; // `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN=0`: the defect.
    }
    let (sb, code, num_fields) = define_finalizable_ctor_class(&shared);
    assert_eq!(code, vec![0x2a, 0x10, 0x10, 0xb7, 0x00, 0x06, 0xb1]);
    let exc_class = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/R21FinalizeThrownRetry", 0)
        .expect("Compatible mode fabricates");
    let ctor_frame = |obj: ObjectRef| {
        Frame::new(
            sb,
            FIN_CLASS.to_string(),
            "<init>".to_string(),
            "()V".to_string(),
            None,
            code.clone(),
            Vec::new(),
            4,
            2,
            &[Value::Object(Some(obj))],
        )
    };
    let registered =
        |addr: usize| shared.mem.ref_processor.lock().is_registered_finalizable(addr);
    // A throw at pc 1 with the constructor as the walk's entry frame.
    let throw_at_entry = |obj: ObjectRef| {
        let exc = shared.mem.heap.alloc_object(exc_class, 0);
        let mut t = JvmThread::new(ThreadId(0x0F1D), "r21-finalize-retry");
        t.frames.push(ctor_frame(obj));
        let mut idx = 0usize;
        assert!(unwind_to_handler(&shared, &mut t, &mut idx, 0, exc, 1).is_err());
    };
    let init_returns_at =
        |obj: ObjectRef, pc: usize| -> bool { finalizer_init_returned(&shared, &ctor_frame(obj), pc) };

    // Case (a).
    let obj = shared.mem.heap.alloc_object(sb, num_fields);
    let addr = obj.as_ptr() as usize;
    shared.register_finalizable(addr);
    throw_at_entry(obj);
    assert!(!registered(addr), "the failed attempt withdrew the registration");
    assert!(!init_returns_at(obj, 1), "pc 1 precedes super(16): no Object.<init> return");
    assert!(!registered(addr));
    assert!(init_returns_at(obj, 6), "the retry's return registers it again");
    assert!(registered(addr));
    assert!(!init_returns_at(obj, 6), "a second return adds no second Finalizer");
    assert!(registered(addr));

    // Case (b): the object's `<init>` returned (it is registered and
    // constructed); a re-`<init>` that throws before `super(16)` keeps it.
    let obj = shared.mem.heap.alloc_object(sb, num_fields);
    let addr = obj.as_ptr() as usize;
    shared.register_finalizable(addr);
    assert!(!init_returns_at(obj, 6), "registered already: nothing to add");
    throw_at_entry(obj);
    assert!(registered(addr), "a constructed object keeps its registration");

    // An object whose `<init>` never returns stays withdrawn.
    let obj = shared.mem.heap.alloc_object(sb, num_fields);
    let addr = obj.as_ptr() as usize;
    shared.register_finalizable(addr);
    throw_at_entry(obj);
    assert!(!registered(addr));

    // A non-constructor frame (the name gate: `make`, and a six-byte
    // `<`-less name) answers nothing.
    for name in ["make", "zinit>"] {
        let other = Frame::new(
            sb,
            FIN_CLASS.to_string(),
            name.to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            Vec::new(),
            4,
            2,
            &[Value::Object(Some(obj))],
        );
        assert!(!finalizer_init_returned(&shared, &other, 0), "{name}");
        assert!(!registered(addr));
    }

    // Wave 19, item 5 of `r21-review-w18-findings-FIXED-20261020.md`: a body
    // that can write local 0 is refused, so a withdrawn object stored
    // into local 0 (`aload_1; astore_0` after `super(16)`) is neither
    // registered again nor marked.
    let reassigned = Frame::new(
        sb,
        FIN_CLASS.to_string(),
        "<init>".to_string(),
        "()V".to_string(),
        None,
        vec![0x2a, 0x10, 0x10, 0xb7, 0x00, 0x06, 0x2b, 0x4b, 0xb1],
        Vec::new(),
        4,
        2,
        &[Value::Object(Some(obj)), Value::Object(Some(obj))],
    );
    assert!(!finalizer_init_returned(&shared, &reassigned, 8), "astore_0: refused");
    assert!(!registered(addr));
    // The same object under the plain body registers again (control).
    assert!(init_returns_at(obj, 6));
    assert!(registered(addr));
    // Wave 19, item 6: the gate's switch and the processor's agree.
    assert_eq!(
        shared.config.finalizer_reregister_at_init_return,
        shared.mem.ref_processor.lock().reregister_at_init_return()
    );
}

/// JIT round 22 wave 1, lane review (`CRATONVM_JNI_REINIT_WITNESS_INTERPRETS`;
/// item 2 of `r21-review-w24-findings-FIXED-20261020.md`): `Object.<init>`'s own
/// frame moves the receiver's witness only while a JNI re-`<init>` window
/// is open, and an open window makes the doors' interpreter-only question
/// answer yes (so they keep that frame instead of eliding it).
#[test]
fn object_init_own_frame_moves_the_witness_only_in_a_window() {
    use std::sync::atomic::Ordering;
    if !finalizer_register_at_object_init_enabled(&crate::config::VmConfig::default()) {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    if !shared.mem.ref_processor.lock().reregister_at_init_return() {
        return;
    }
    let (sb, _code, num_fields) = define_finalizable_ctor_class(&shared);
    let obj = shared.mem.heap.alloc_object(sb, num_fields);
    let addr = obj.as_ptr() as usize;
    shared.register_finalizable(addr);
    let object_init = Frame::new(
        sb,
        "java/lang/Object".to_string(),
        "<init>".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        Vec::new(),
        4,
        2,
        &[Value::Object(Some(obj))],
    );
    let witness = || shared.mem.ref_processor.lock().finalizable_init_returns(addr);
    let before = witness().expect("an active row");
    assert!(!jni_reinit_witness_window_open(&shared));
    let _ = finalizer_init_returned(&shared, &object_init, 0);
    assert_eq!(witness(), Some(before), "closed window: not counted (no invokespecial)");
    shared.jni_reinit_witness_window.fetch_add(1, Ordering::AcqRel);
    assert!(jni_reinit_witness_window_open(&shared));
    assert!(
        super::super::jit_bridge::jvmti_requires_interpreter(&shared),
        "an open window keeps the doors interpreted and framed"
    );
    let _ = finalizer_init_returned(&shared, &object_init, 0);
    assert_eq!(witness(), Some(before.wrapping_add(1)), "open window: Object.<init> returned");
    shared.jni_reinit_witness_window.fetch_sub(1, Ordering::AcqRel);
    assert!(!jni_reinit_witness_window_open(&shared));
}

/// Wave 19 (items 5 and 6 of `r21-review-w18-findings-FIXED-20261020.md`): the
/// hook's lock-free body check. It refuses every local-0 write and a
/// `return` before any `invokespecial`, and accepts a `return` after one.
#[test]
fn the_init_return_check_refuses_local_0_writes_and_an_early_return() {
    let frame = |code: Vec<u8>| {
        Frame::new(
            ClassId::new(0x00F1_0C77),
            "cratonvm/test/W19Ctor".to_string(),
            "<init>".to_string(),
            "()V".to_string(),
            None,
            code,
            Vec::new(),
            4,
            3,
            &[],
        )
    };
    let check = |code: Vec<u8>, pc: usize| {
        ctor_return_names_its_initialized_receiver(&frame(code), pc)
    };
    // aload_0; invokespecial #1; return
    assert!(check(vec![0x2a, 0xb7, 0, 1, 0xb1], 4));
    // A bare `return` (unverified: Object.<init> never ran).
    assert!(!check(vec![0xb1], 0));
    // A return before the invokespecial.
    assert!(!check(vec![0xb1, 0x2a, 0xb7, 0, 1, 0xb1], 0));
    assert!(check(vec![0xb1, 0x2a, 0xb7, 0, 1, 0xb1], 5));
    // Writes to local 0: astore_0, astore 0, wide astore 0, iinc 0.
    assert!(!check(vec![0x2a, 0xb7, 0, 1, 0x2b, 0x4b, 0xb1], 6));
    assert!(!check(vec![0x2a, 0xb7, 0, 1, 0x2b, 0x3a, 0, 0xb1], 7));
    assert!(!check(vec![0x2a, 0xb7, 0, 1, 0x2b, 0xc4, 0x3a, 0, 0, 0xb1], 9));
    assert!(!check(vec![0x2a, 0xb7, 0, 1, 0x84, 0, 1, 0xb1], 7));
    // A store to another local is fine.
    assert!(check(vec![0x2a, 0xb7, 0, 1, 0x2b, 0x4c, 0xb1], 6));
    // Wave 21: subroutines are refused (jsr, jsr_w, ret, wide ret).
    assert!(!check(vec![0x2a, 0xb7, 0, 1, 0xa8, 0, 3, 0xb1], 7));
    assert!(!check(vec![0x2a, 0xb7, 0, 1, 0xa9, 1, 0xb1], 6));
    assert!(!check(vec![0x2a, 0xb7, 0, 1, 0xc4, 0xa9, 0, 1, 0xb1], 8));
}

/// Wave 21 (item 8 of `r21-review-w21-findings-FIXED-20261020.md`): JEP 513
/// constructor prologues. A `try`/`catch` before `super(..)` (the
/// unwinder's prefix scan refused a handler that starts before the first
/// `invokespecial` until wave 26, and still does under
/// `CRATONVM_FINALIZER_CTOR_PROLOGUE_HANDLERS=0`) and a loop before it
/// are accepted by the `<init>` return check, which asks only about local 0 and subroutines; switches
/// decode.
#[test]
fn the_init_return_check_accepts_jep_513_prologues() {
    use cratonvm_reader::attribute::ExceptionTableEntry;
    // 0 invokestatic #2; 3 goto 7; 6 astore_1 (handler of [0, 3));
    // 7 aload_0; 8 invokespecial #1; 11 return
    let try_prologue = vec![0xb8, 0, 2, 0xa7, 0, 4, 0x4c, 0x2a, 0xb7, 0, 1, 0xb1];
    let table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 3,
        handler_pc: 6,
        catch_type: 0,
    }];
    let frame = Frame::new(
        ClassId::new(0x00F1_0C78),
        "cratonvm/test/W21Prologue".to_string(),
        "<init>".to_string(),
        "()V".to_string(),
        None,
        try_prologue.clone(),
        table.clone(),
        4,
        3,
        &[],
    );
    assert!(
        ctor_prefix_scan(&try_prologue, &table, false, &|_| true).is_none(),
        "the withdraw side's scan refuses it with CRATONVM_FINALIZER_CTOR_PROLOGUE_HANDLERS=0"
    );
    // Wave 26 (lane finprologue): the row covers only [0, 3), before the
    // `invokespecial` at 8, so the scan accepts it by default.
    assert_eq!(
        ctor_prefix_scan(&try_prologue, &table, true, &|_| true).map(|(first, _)| first),
        Some(8)
    );
    assert!(ctor_return_names_its_initialized_receiver(&frame, 11), "the return check accepts it");
    assert!(!ctor_return_names_its_initialized_receiver(&frame, 7), "before super(..)");

    // 0 iconst_0; 1 istore_1; 2 iload_1; 3 iconst_3; 4 if_icmpge 13;
    // 7 iinc 1 1; 10 goto 2; 13 aload_0; 14 invokespecial #1; 17 return
    let loop_prologue = vec![
        0x03, 0x3c, 0x1b, 0x06, 0xa2, 0, 9, 0x84, 1, 1, 0xa7, 0xff, 0xf8, 0x2a, 0xb7, 0, 1, 0xb1,
    ];
    assert!(ctor_body_keeps_receiver_and_calls_before(&loop_prologue, 17));
    // 0 iload_1; 1 tableswitch (pad 2) default/low/high = 0..0 -> 1 target;
    // then aload_0; invokespecial #1; return.
    let mut switch = vec![0x1b, 0xaa, 0, 0];
    for word in [23i32, 0, 0, 23] {
        switch.extend_from_slice(&word.to_be_bytes());
    }
    switch.extend_from_slice(&[0, 0, 0, 0x2a, 0xb7, 0, 1, 0xb1]);
    let ret = switch.len() - 1;
    assert!(ctor_body_keeps_receiver_and_calls_before(&switch, ret));
    // A truncated body is refused.
    assert!(!ctor_body_keeps_receiver_and_calls_before(&[0x2a, 0xb7, 0], 3));
}

/// Wave 26 (lane finprologue, `CRATONVM_FINALIZER_CTOR_PROLOGUE_HANDLERS`;
/// probe `R21Fz8FinPrologue1`): a JEP 513 `try`/`catch` before
/// `super(..)` no longer refuses the unwinder's proof, so a throw before
/// `super(..)` is proven (and the constructor stays interpreted); a row
/// that covers the initializing call, or sends control from past it back
/// below it, still refuses.
#[test]
fn the_unwinder_proof_accepts_a_jep_513_prologue_handler() {
    use cratonvm_reader::attribute::ExceptionTableEntry;
    // 0 invokestatic #2; 3 istore_3; 4 goto 11; 7 astore 4 (handler);
    // 9 iconst_m1; 10 istore_3; 11 iload_2; 12 iconst_1; 13 if_icmpne 24;
    // 16 new #5; 19 dup; 20 invokespecial #5 (another class's <init>);
    // 23 athrow; 24 aload_0; 25 iload_3; 26 invokespecial #1; 29 return
    let code = vec![
        0xb8, 0, 2, 0x3e, 0xa7, 0, 7, 0x3a, 4, 0x02, 0x3e, 0x1c, 0x04, 0xa0, 0, 11, 0xbb, 0, 5,
        0x59, 0xb7, 0, 5, 0xbf, 0x2a, 0x1d, 0xb7, 0, 1, 0xb1,
    ];
    let may = |i: u16| i != 5;
    let row = |start_pc: u16, end_pc: u16, handler_pc: u16| ExceptionTableEntry {
        start_pc,
        end_pc,
        handler_pc,
        catch_type: 0,
    };
    let prologue = [row(0, 3, 7)];
    assert!(ctor_throw_precedes_initialization_table(&code, 23, &prologue, true, &may));
    assert!(ctor_throw_precedes_initialization_table(&code, 0, &prologue, true, &may));
    assert!(!ctor_throw_precedes_initialization_table(&code, 23, &prologue, false, &may), "=0");
    assert!(!ctor_throw_precedes_initialization_table(&code, 29, &prologue, true, &may), "after");
    assert!(ctor_may_throw_before_initialization_table(&code, &prologue, true, &may));
    assert!(!ctor_may_throw_before_initialization_table(&code, &prologue, false, &may));
    // A row that covers the initializing call (its handler would run on
    // an object whose super(..) threw) refuses.
    let covers_call = [row(0, 29, 7)];
    assert!(!ctor_throw_precedes_initialization_table(&code, 23, &covers_call, true, &may));
    // A row past the call whose handler lies before it refuses.
    let backward = [row(29, 30, 7)];
    assert!(!ctor_throw_precedes_initialization_table(&code, 23, &backward, true, &may));
    // A forward handler is allowed either way (the pre-wave-26 rule).
    let forward = [row(0, 3, 29)];
    assert!(ctor_throw_precedes_initialization_table(&code, 23, &forward, false, &may));
    // An empty row is never an allowance.
    let empty = [row(3, 3, 7)];
    assert!(!ctor_throw_precedes_initialization_table(&code, 23, &empty, true, &may));
}

/// Wave 22 (lane finalize; item 8 of `r21-review-w21-findings-FIXED-20261020.md`,
/// `CRATONVM_FINALIZER_INIT_RETURN_SCAN_MEMO`): the memoized answer is the
/// decode's at every return pc; the cell on the frame's quickened stream is
/// filled once and then read instead of decoding; a stream of other bytes
/// (a redefined body) is never the frame's cell; the switch off decodes.
#[test]
fn the_init_return_scan_memo_agrees_with_the_decode() {
    use std::sync::Arc;
    let bodies: [&[u8]; 7] = [
        &[0x2a, 0xb7, 0, 1, 0xb1],
        &[0xb1, 0x2a, 0xb7, 0, 1, 0xb1],
        &[0x2a, 0xb7, 0, 1, 0x2b, 0x4b, 0xb1],
        &[0x2a, 0xb7, 0, 1, 0xa9, 1, 0xb1],
        &[0xb1],
        &[0x2a, 0xb7, 0],
        &[0x2a, 0xb7, 0, 1, 0x2a, 0xb7, 0, 2, 0xb1],
    ];
    for body in bodies {
        let facts = ctor_body_receiver_facts(body);
        assert_ne!(facts, CTOR_FACTS_UNKNOWN, "{body:?}");
        for pc in 0..=body.len() + 1 {
            assert_eq!(
                ctor_facts_answer(facts, pc),
                ctor_body_keeps_receiver_and_calls_before(body, pc),
                "{body:?} at {pc}"
            );
        }
    }
    let padded = |body: &[u8]| -> Arc<[u8]> {
        let mut v = body.to_vec();
        v.extend_from_slice(&[0, 0]);
        Arc::from(v.into_boxed_slice())
    };
    let record = |code: Arc<[u8]>, stream_of: &Arc<[u8]>| {
        let cached = Arc::new(crate::classloading::resolution::CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(0x00F1_0C79),
                class_name: Arc::from("cratonvm/test/W22Memo"),
                method_name: Arc::from("<init>"),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                code,
                exception_table: Arc::from(Vec::new()),
                max_stack: 4,
                max_locals: 3,
                num_params: 0,
                is_synchronized: false,
                is_static: false,
            },
        ));
        let stream = cratonvm_reader::QuickenedCode::build(stream_of).expect("decodes");
        assert!(cached.quickened.set(Some(Arc::clone(&stream))).is_ok());
        (cached, stream)
    };
    // 0 return; 1 aload_0; 2 invokespecial #1; 5 return
    let code = padded(&[0xb1, 0x2a, 0xb7, 0, 1, 0xb1]);
    let (cached, stream) = record(Arc::clone(&code), &code);
    let frame = Frame::new_cached_unpooled(cached, &[]);
    assert!(ctor_receiver_facts_cell(&frame).is_some(), "the record's own stream");
    assert_eq!(stream.ctor_receiver_facts(), CTOR_FACTS_UNKNOWN);
    assert!(!ctor_return_names_its_initialized_receiver_in(&frame, 0, true));
    assert_eq!(stream.ctor_receiver_facts(), 2 + 2, "the first invokespecial is at pc 2");
    assert!(ctor_return_names_its_initialized_receiver_in(&frame, 5, true));
    // A poisoned cell answers: the memo is read, not a decode.
    stream.set_ctor_receiver_facts(CTOR_FACTS_REFUSED);
    assert!(!ctor_return_names_its_initialized_receiver_in(&frame, 5, true));
    assert!(
        ctor_return_names_its_initialized_receiver_in(&frame, 5, false),
        "the switch off decodes"
    );
    // A stream that pins other bytes (equal or not) is not this frame's cell.
    let other = padded(&[0xb1, 0x2a, 0xb7, 0, 1, 0xb1]);
    let (cached2, _) = record(Arc::clone(&code), &other);
    let frame2 = Frame::new_cached_unpooled(cached2, &[]);
    assert!(ctor_receiver_facts_cell(&frame2).is_none());
    assert!(ctor_return_names_its_initialized_receiver_in(&frame2, 5, true), "decodes");
}

/// Wave 19 source pin (item 6): the `return` arm's gate bit is armed by
/// both switches.
#[test]
fn the_init_return_gate_bit_is_armed_by_the_reregister_switch() {
    let src = concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    let at = src.find("DG_FINALIZER_INIT_RETURN,\n").expect("the gate word row");
    let row = &src[src[..at].rfind("(\n").expect("the row's open paren")..at];
    assert!(row.contains("finalizer_register_at_object_init_enabled(&shared.config)"));
    assert!(row.contains("shared.config.finalizer_reregister_at_init_return"));
}

/// Wave 14 (`R21Fz3FinCtorChain1` `holderCatchInner`): an interpreted
/// creator whose `try` covers `new Bf2(v)` (javac: the reference only on
/// the operand stack) withdraws the unconstructed object's registration;
/// the same creator with the object in a local keeps it (the retry shape).
#[test]
fn a_covered_creator_withdraws_unless_a_local_holds_the_object() {
    use crate::threading::jvm_thread::ThreadId;
    use cratonvm_reader::attribute::ExceptionTableEntry;
    if !finalizer_register_at_object_init_enabled(&crate::config::VmConfig::default()) {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    // Wave 15: a hand-built finalizable class (the synthetic JDK's
    // `StringBuilder` has no `()V` bytecode).
    let (sb, code, num_fields) = define_finalizable_ctor_class(&shared);
    if code.len() != 7 || code[0] != 0x2a || code[1] != 0x10 || code[3] != 0xb7 {
        return;
    }
    let exc_class = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/R21FinalizeThrownCovered", 0)
        .expect("Compatible mode fabricates");
    let registered_after = |holds: bool| -> bool {
        let obj = shared.mem.heap.alloc_object(sb, num_fields);
        let addr = obj.as_ptr() as usize;
        shared.register_finalizable(addr);
        let exc = shared.mem.heap.alloc_object(exc_class, 0);
        let mut t = JvmThread::new(ThreadId(0x0F1E), "r21-finalize-covered");
        let held = [Value::Object(Some(obj))];
        // The creator: `invokespecial #1; return; athrow`, a row over the
        // invoke with its handler at the `athrow`.
        t.frames.push(Frame::new(
            ClassId::new(0x00F1_0C05),
            "cratonvm/test/R21CoveredCreator".to_string(),
            "make".to_string(),
            "()V".to_string(),
            None,
            vec![0xb7, 0, 1, 0xb1, 0xbf],
            vec![ExceptionTableEntry {
                start_pc: 0,
                end_pc: 3,
                handler_pc: 4,
                catch_type: 0,
            }],
            4,
            2,
            if holds { &held[..] } else { &[] },
        ));
        t.frames[0].last_instr_pc = 0;
        t.frames.push(Frame::new(
            sb,
            FIN_CLASS.to_string(),
            "<init>".to_string(),
            "()V".to_string(),
            None,
            code.clone(),
            Vec::new(),
            4,
            2,
            &[Value::Object(Some(obj))],
        ));
        let mut idx = 1usize;
        // The creator's catch-all row catches it: `Ok`, handler installed.
        assert!(unwind_to_handler(&shared, &mut t, &mut idx, 0, exc, 1).is_ok());
        assert_eq!(idx, 0, "caught in the creator");
        shared.mem.ref_processor.lock().is_registered_finalizable(addr)
    };
    assert!(!registered_after(false), "javac's shape: withdrawn although caught");
    assert!(registered_after(true), "a local could retry <init>: kept");
}

/// Wave 17 (lane finalize-reinit, items 1 and 2 of
/// `r21-review-w16-findings-FIXED-20261020.md`), on a hand-built hierarchy
/// `Root > Quiet > Base > Fin > Leaf`. `Base(String)` throws before its
/// initializing call; `Quiet` has no constructor that can; `Fin` declares
/// a finalizer and `Leaf` inherits it. Only `Leaf` reaches the listener
/// (`Fin` was "loaded as its superclass").
/// - Item 1: the wave-13 walk stopped at `Fin` and recorded nothing; the
///   walk past it records `Base`, and the bar refuses `Base(String)`.
/// - Item 2: `Quiet` is recorded although nothing in it throws now; once
///   its constructor is replaced by a throwing one (a redefinition), the
///   bar refuses it with no further define.
#[test]
fn a_finalizable_superclass_does_not_end_the_ancestor_walk() {
    use crate::classloading::{Class, ClassLoaderId, ClassState};
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::method::ClassFileMethod;
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let config = &shared.config;
    if !finalizer_register_at_object_init_enabled(config)
        || !finalizer_ctor_stays_interpreted_enabled(config)
        || !finalizer_ancestor_ctor_stays_interpreted_enabled(config)
    {
        return; // A kill switch: the residual.
    }
    const THROWS: &str = "(Ljava/lang/String;)V";
    // `aconst_null; athrow`: throws before any initializing call.
    let throwing_ctor = || ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: cratonvm_types::intern_arc("<init>"),
        descriptor: cratonvm_types::intern_arc(THROWS),
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(CodeAttribute {
            max_stack: 1,
            max_locals: 2,
            code: cratonvm_reader::ByteView::from_vec(vec![0x01, 0xbf]),
            exception_table: vec![],
            attributes: vec![],
        })))],
    };
    let class = |id: ClassId,
                 name: &str,
                 superclass: Option<ClassId>,
                 methods: Vec<ClassFileMethod>,
                 has_finalizer: bool| Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Initialized,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass,
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
        origin: cratonvm_classloading::ClassOrigin::default(),
        has_finalizer,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    };
    let (quiet, base, fin, leaf) = {
        let mut cm = shared.classes.class_manager.write();
        let mut add = |name: &str, sup: Option<ClassId>, m: Vec<ClassFileMethod>, f: bool| {
            let id = cm.class_store.next_id();
            cm.class_store.add(class(id, name, sup, m, f));
            id
        };
        let root = add("cratonvm/test/W17Root", None, vec![], false);
        let quiet = add("cratonvm/test/W17Quiet", Some(root), vec![], false);
        let base = add("cratonvm/test/W17Base", Some(quiet), vec![throwing_ctor()], false);
        let fin = add("cratonvm/test/W17Fin", Some(base), vec![], true);
        let leaf = add("cratonvm/test/W17Leaf", Some(fin), vec![], true);
        (quiet, base, fin, leaf)
    };
    let walk = |past: bool| {
        let cm = shared.classes.class_manager.read();
        finalizable_define_ancestors(&shared.config, past, cm.class_store(), leaf)
    };
    assert!(walk(false).is_empty(), "wave 13: the walk stopped at the finalizable Fin");
    let reached = walk(true);
    assert_eq!(
        reached.iter().map(|a| (a.id, a.stale)).collect::<Vec<_>>(),
        vec![(base, true), (quiet, false)],
        "past Fin, up to (not including) the root: {reached:?}"
    );
    assert!(reached.iter().all(|a| a.id != fin && a.id != leaf));
    assert!(!finalizer_bars_compiling(&shared, base, "<init>", THROWS), "unrecorded yet");

    shared.note_finalizable_define(leaf);
    if shared.finalizer_ancestor_walk_past_finalizable {
        assert!(
            finalizer_bars_compiling(&shared, base, "<init>", THROWS),
            "item 1: Base(String) stays interpreted"
        );
    }
    if !shared.finalizer_ancestor_record_reachable {
        return;
    }
    assert!(
        shared
            .classes
            .class_manager
            .read()
            .class_store()
            .is_finalizable_ancestor(quiet),
        "item 2: reachable, recorded although nothing throws"
    );
    assert!(!finalizer_bars_compiling(&shared, quiet, "<init>", THROWS), "no such ctor yet");
    // A redefinition replaces Quiet's methods with a throwing constructor.
    shared
        .classes
        .class_manager
        .write()
        .class_store
        .get_mut(quiet)
        .expect("defined")
        .methods = vec![throwing_ctor()];
    assert!(
        finalizer_bars_compiling(&shared, quiet, "<init>", THROWS),
        "item 2: the bar reads the redefined bytes"
    );
}


/// Wave 13 (residual 1 of the page): `IndexOutOfBoundsException(int)`
/// (`super("Index out of range: " + index)`, an `invokedynamic` before
/// `super(..)`) has no finalizer of its own. The define of a finalizable
/// subclass (here `ArrayIndexOutOfBoundsException`, made finalizable in
/// this VM's store only) names it as stale, and once the listener's
/// record is made (`ClassStore::mark_finalizable_ancestor`) it stays
/// interpreted; `IndexOutOfBoundsException(String)` (`super(s)`) still
/// compiles. Before the record the bar answers from the class alone
/// (review rev-jit w13 item 3b: no prefix decode, no subtree walk).
#[test]
fn an_ancestor_constructor_of_a_loaded_finalizable_class_stays_interpreted() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let config = &shared.config;
    if !finalizer_register_at_object_init_enabled(config)
        || !finalizer_ctor_stays_interpreted_enabled(config)
        || !finalizer_ancestor_ctor_stays_interpreted_enabled(config)
    {
        return; // A kill switch: the residual.
    }
    const BASE: &str = "java/lang/IndexOutOfBoundsException";
    let leaf = shared
        .load_class_concurrent("java/lang/ArrayIndexOutOfBoundsException")
        .expect("load ArrayIndexOutOfBoundsException");
    let shape = {
        let cm = shared.classes.class_manager.read();
        let store = cm.class_store();
        store
            .get(leaf)
            .and_then(|c| c.superclass)
            .and_then(|id| store.get(id).map(|b| (id, b)))
            .filter(|(_, b)| &*b.name == BASE && !b.has_finalizer)
            .map(|(id, b)| {
                (
                    id,
                    class_ctor_may_throw_before_initialization(store, b, "(I)V"),
                    class_ctor_may_throw_before_initialization(
                        store,
                        b,
                        "(Ljava/lang/String;)V",
                    ),
                )
            })
    };
    let Some((base, int_may_throw, string_may_throw)) = shape else {
        return; // Another JDK's hierarchy.
    };
    if !int_may_throw || string_may_throw {
        return; // Another JDK's bodies: the shapes this test names are gone.
    }
    let stale_now = || {
        let cm = shared.classes.class_manager.read();
        finalizable_define_stale_ancestors(&shared.config, cm.class_store(), leaf)
    };
    assert!(
        !finalizer_bars_compiling(&shared, base, "<init>", "(I)V"),
        "no finalizable subclass defined yet"
    );
    assert!(stale_now().is_empty(), "the leaf has no finalizer yet");

    shared
        .classes
        .class_manager
        .write()
        .class_store
        .get_mut(leaf)
        .expect("loaded")
        .has_finalizer = true;

    let stale = stale_now();
    assert!(
        stale.iter().any(|(id, name)| *id == base && name == BASE),
        "the define names the ancestor: {stale:?}"
    );
    assert!(
        stale.iter().all(|(id, name)| *id != leaf && name != "java/lang/Object"),
        "neither the new class itself nor java/lang/Object: {stale:?}"
    );
    assert!(
        !finalizer_bars_compiling(&shared, base, "<init>", "(I)V"),
        "unrecorded: the bar reads the class store's record, not the subtree"
    );
    {
        // The listener's record (`vm_init.rs`).
        let mut cm = shared.classes.class_manager.write();
        for (id, _) in &stale {
            assert!(cm.class_store.mark_finalizable_ancestor(*id));
        }
    }
    assert!(
        finalizer_bars_compiling(&shared, base, "<init>", "(I)V"),
        "throws before super(..) and reaches a finalizable object"
    );
    assert!(
        !finalizer_bars_compiling(&shared, base, "<init>", "(Ljava/lang/String;)V"),
        "super(s) cannot throw before it"
    );
    assert!(!finalizer_bars_compiling(&shared, base, "toString", "()Ljava/lang/String;"));
}

/// Wave 13 (review rev-jit w13 item 2): the entry is a JNI native's own
/// upcall only at the row's interpreter depth with the row's JIT chain.
#[test]
fn a_jni_upcall_entry_is_the_innermost_row_at_its_depth() {
    use crate::threading::jvm_thread::ThreadId;
    let mut t = JvmThread::new(ThreadId(0x0F1C), "r21-finalize-jni-row");
    assert!(!entry_is_jni_upcall(&t, 1, 0), "no JNI native runs");
    t.jni_native_frames.push((1, ClassId::new(0x00F1_0C03), 0x1000, 0));
    assert!(entry_is_jni_upcall(&t, 1, 0));
    assert!(!entry_is_jni_upcall(&t, 2, 0), "an interpreted frame in between");
    assert!(!entry_is_jni_upcall(&t, 1, 1), "a compiled method in between");
    t.jni_native_frames.push((3, ClassId::new(0x00F1_0C03), 0x1000, 0));
    assert!(!entry_is_jni_upcall(&t, 1, 0), "only the innermost native counts");
    assert!(entry_is_jni_upcall(&t, 3, 0));
}

/// Wave 13 (review rev-jit w13 item 2, page
/// `r21-jni-reinit-entry-withdraw-drops-registration-20261019.md`): a JNI
/// `<init>` call on an object some earlier construction registered, whose
/// constructor throws before `super(..)`, keeps the registration (HotSpot
/// finalizes the object once). The walk's entry is the JNI upcall, so it
/// only notes the object; the note is what the JNI door takes when the
/// registration is its own call's (`jni_constructor_call_withdrawable`).
#[test]
fn a_jni_reinit_upcall_keeps_an_earlier_registration() {
    use crate::threading::jvm_thread::ThreadId;
    if !finalizer_register_at_object_init_enabled(&crate::config::VmConfig::default()) {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    // Wave 15: a hand-built finalizable class (the synthetic JDK's
    // `StringBuilder` has no `()V` bytecode).
    let (sb, code, num_fields) = define_finalizable_ctor_class(&shared);
    if code.len() != 7 || code[0] != 0x2a || code[1] != 0x10 || code[3] != 0xb7 {
        return;
    }
    let exc_class = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/R21FinalizeThrownJni", 0)
        .expect("Compatible mode fabricates");
    let obj = shared.mem.heap.alloc_object(sb, num_fields);
    let addr = obj.as_ptr() as usize;
    shared.register_finalizable(addr);
    let epoch = shared.mem.address_epoch();
    let exc = shared.mem.heap.alloc_object(exc_class, 0);
    let mut t = JvmThread::new(ThreadId(0x0F1D), "r21-finalize-jni-reinit");
    // The Java frame that called the JNI native: `invokestatic #1; return`.
    t.frames.push(Frame::new(
        ClassId::new(0x00F1_0C04),
        "cratonvm/test/R21JniCaller".to_string(),
        "callNative".to_string(),
        "()V".to_string(),
        None,
        vec![0xb8, 0, 1, 0xb1],
        Vec::new(),
        4,
        2,
        &[],
    ));
    t.frames[0].last_instr_pc = 0;
    let jit_depth = crate::jit::conservative_roots::current_thread_jit_depth();
    let row_jit_depth = u32::try_from(jit_depth).unwrap_or(u32::MAX);
    t.jni_native_frames
        .push((1, ClassId::new(0x00F1_0C04), 0x2000, row_jit_depth));
    t.frames.push(Frame::new(
        sb,
        FIN_CLASS.to_string(),
        "<init>".to_string(),
        "()V".to_string(),
        None,
        code.clone(),
        Vec::new(),
        4,
        2,
        &[Value::Object(Some(obj))],
    ));
    let mut idx = 1usize;
    assert!(unwind_to_handler(&shared, &mut t, &mut idx, 1, exc, 1).is_err());
    assert!(
        shared.mem.ref_processor.lock().is_registered_finalizable(addr),
        "the earlier construction's registration stays"
    );
    // The door of a call whose registration WAS its own takes the note.
    assert!(
        shared.withdraw_unconstructed_finalizable_noted(addr, epoch),
        "noted for the JNI door"
    );
}

/// Wave 14 (items 2 and 3 of `r21-review-w14-findings-20261019.md`): the
/// gate's refusal is memoized per class id, not sealed by name, and the
/// ancestor record the gate re-reads after a memo fill is what
/// `mark_finalizable_ancestor` set.
///
/// JIT round 22 wave 1, lane review (item 5 of
/// `r21-review-w25-findings-FIXED-20261020.md`): both arms of the opt-in
/// `CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH` run, each on its own VM with the
/// per-VM latch pinned, so the switch-on branch is tested in a default
/// `cargo test` too (it ran only with the variable in the environment).
#[test]
fn the_gate_memoizes_a_refusal_per_class_id_and_rereads_the_record() {
    for epoch_switch in [false, true] {
        let mut vm = SharedVm::new(crate::config::VmConfig::default());
        vm.finalizer_bar_refusal_epoch = epoch_switch;
        the_gate_memoizes_a_refusal_round(Arc::new(vm));
    }
}

/// One VM's round of
/// [`the_gate_memoizes_a_refusal_per_class_id_and_rereads_the_record`].
fn the_gate_memoizes_a_refusal_round(shared: Arc<SharedVm>) {
    let config = &shared.config;
    if !finalizer_register_at_object_init_enabled(config)
        || !finalizer_ctor_stays_interpreted_enabled(config)
        || !finalizer_ancestor_ctor_stays_interpreted_enabled(config)
    {
        return;
    }
    let leaf = shared
        .load_class_concurrent("java/lang/ArrayIndexOutOfBoundsException")
        .expect("load ArrayIndexOutOfBoundsException");
    let other = shared
        .load_class_concurrent("java/lang/StringBuilder")
        .expect("load StringBuilder");
    let base = {
        let cm = shared.classes.class_manager.read();
        let store = cm.class_store();
        let Some(base) = store.get(leaf).and_then(|c| c.superclass) else {
            return;
        };
        let throws = store.get(base).is_some_and(|b| {
            &*b.name == "java/lang/IndexOutOfBoundsException"
                && !b.has_finalizer
                && class_ctor_may_throw_before_initialization(store, b, "(I)V")
        });
        if !throws {
            return; // Another JDK's hierarchy or body.
        }
        base
    };
    assert!(!finalizer_ancestor_recorded(&shared, base));
    assert!(!finalizer_bars_compiling_memoized(&shared, base, "<init>", "(I)V"));
    assert!(
        shared.jit.finalizer_bar_refusals.read().is_empty(),
        "an allowed answer records nothing"
    );
    shared
        .classes
        .class_manager
        .write()
        .class_store
        .get_mut(leaf)
        .expect("loaded")
        .has_finalizer = true;
    assert!(shared
        .classes
        .class_manager
        .write()
        .class_store
        .mark_finalizable_ancestor(base));
    assert!(finalizer_ancestor_recorded(&shared, base), "the gate's re-read sees the record");
    assert!(finalizer_bars_compiling_memoized(&shared, base, "<init>", "(I)V"));
    assert!(finalizer_bars_compiling_memoized(&shared, base, "<init>", "(I)V"), "answered from the row");
    {
        let refusals = shared.jit.finalizer_bar_refusals.read();
        assert_eq!(refusals.len(), 1, "keyed by the declaring class id only");
        assert_eq!(
            refusals
                .get(&base)
                .map(|(_, d)| d.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            Some(vec!["(I)V".to_string()])
        );
    }
    assert!(!finalizer_bars_compiling_memoized(&shared, other, "<init>", "()V"));
    assert!(!finalizer_bars_compiling_memoized(&shared, base, "toString", "()Ljava/lang/String;"));

    // Wave 18 (`CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH`): the row carries
    // the redefine epoch it was filled in. Simulate a redefinition that
    // makes the constructor unable to throw before init: under the
    // switch the old row no longer answers; off, it still does.
    let filled = shared.jit.jit_cache.redefine_epoch();
    assert_eq!(shared.jit.finalizer_bar_refusals.read().get(&base).map(|(e, _)| *e), Some(filled));
    shared
        .classes
        .class_manager
        .write()
        .class_store
        .get_mut(base)
        .expect("loaded")
        .methods
        .retain(|m| !(&*m.name == "<init>" && &*m.descriptor == "(I)V"));
    shared.jit.jit_cache.bump_redefine_epoch();
    assert_eq!(
        finalizer_bars_compiling_memoized(&shared, base, "<init>", "(I)V"),
        !shared.finalizer_bar_refusal_epoch,
        "a stale row answers only without the switch"
    );
    // Wave 24 (lane flipprep, flip review item 8): under the switch the
    // allowed re-ask drops the stale descriptor, and the row with it, so
    // later visits do not re-ask through it; off, the row stays as filled.
    assert_eq!(
        shared.jit.finalizer_bar_refusals.read().get(&base).map(|(e, _)| *e),
        if shared.finalizer_bar_refusal_epoch { None } else { Some(filled) },
        "an allowed re-ask clears the stale row only under the switch"
    );
    if shared.finalizer_bar_refusal_epoch {
        assert!(!finalizer_bars_compiling_memoized(&shared, base, "<init>", "(I)V"));
        assert!(shared.jit.finalizer_bar_refusals.read().get(&base).is_none());
    }
}

/// Wave 14 source pins (`interpreter.rs`): the refusal is no longer
/// sealed into the name-keyed `jit_skip_set`; an allowed `jit_gate_pass`
/// fill for a constructor re-reads the ancestor record and drops its row;
/// the eager first-call door asks the bar again before its `put`.
#[test]
fn the_execute_gate_rereads_the_record_and_seals_no_finalizer_refusal() {
    let src = concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    let seal = src
        .find("if static_skip_reason.is_some() || fjp_skip || native_skip || clinit_skip {")
        .expect("the static seal");
    let seal_end = seal + src[seal..].find("\n        } else {").expect("its end");
    assert!(
        !src[seal..seal_end].contains("finalizer_skip"),
        "a finalizer refusal must not be sealed by class name"
    );
    let fill = src
        .find("shared.jit.jit_gate_pass.write().insert(")
        .expect("the gate-pass fill");
    let after = &src[fill..];
    let reread = after
        .find("finalizer_ancestor_recorded(shared, class_id)")
        .expect("the fill re-reads the ancestor record");
    assert!(reread < 2000, "right after the insert");
    let drop_at = after[reread..]
        .find("shared.jit.jit_gate_pass.write().remove(&gate_pass_key)")
        .expect("and drops the row");
    assert!(drop_at < 400, "in the same block");
    let put = src.find("let stale = crate::jit::helpers::varhandle_thin_bound_artifact_is_stale(")
        .expect("the eager door's stale test");
    let tail = &src[put..];
    let asked = tail.find("|| finalizer_bars_compiling(").expect("the eager door asks the bar again");
    let put_at = tail.find("jit_cache.put(").expect("the eager door's put");
    assert!(asked < put_at && put_at < 4000, "before its put");
}
