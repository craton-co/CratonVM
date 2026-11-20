//! Round 14 wave 2 (lane deopt; proposal R13RP6-1): a trap out of a
//! SUPERSEDED body of a redefined class, stashed by the x64 framed
//! trampoline, carries the bytecode that body was compiled from, and the
//! call-site service resumes it there (restamped), not in the new body.
use super::*;
use crate::config::VmConfig;
use crate::types::Value;
use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};
use cratonvm_jit::deopt::{
    DeoptAction, DeoptEpochGuard, DeoptReason, DeoptimizationPoint, FrameState,
    ResumeSemantics, SavedRegisters,
};

/// `obsolete/VmProbe` with `static value()I` = `ldc #8; ireturn`, `#8`
/// the Integer `constant` (the fixture of `obsolete_frames`' tests).
fn ldc_class(constant: i32) -> Vec<u8> {
    probe_class("obsolete/VmProbe", constant)
}

/// [`ldc_class`] named `name`.
fn probe_class(name: &str, constant: i32) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let code: &[u8] = &[0x12, 8, 0xac];
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&9u16.to_be_bytes());
    utf8(&mut b, name); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, "value"); // #5
    utf8(&mut b, "()I"); // #6
    utf8(&mut b, "Code"); // #7
    b.push(3); // #8 Integer
    b.extend_from_slice(&constant.to_be_bytes());
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
    b.extend_from_slice(&[0, 1]); // methods_count
    b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
    b.extend_from_slice(&[0, 7]); // "Code"
    b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
    b.extend_from_slice(&(code.len() as u32).to_be_bytes());
    b.extend_from_slice(code);
    b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
    b.extend_from_slice(&[0, 0]); // no class attributes
    b
}

/// `value()`'s current body as a publish site's template.
fn value_template(shared: &SharedVm, cid: ClassId) -> Option<Arc<CachedBytecodeMethod>> {
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(cid)?;
    let method = class.methods.iter().find(|m| &*m.name == "value")?;
    let code = method.code()?;
    Some(Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: cid,
            class_name: Arc::clone(&class.name),
            method_name: Arc::from("value"),
            method_descriptor: Arc::from("()I"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(&code.code[..]),
            exception_table: Arc::from(
                Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new(),
            ),
            max_stack: code.max_stack,
            max_locals: code.max_locals,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    )))    }

/// A re-execute guard at `value()`'s first instruction.
fn guard_at_entry() -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0,
        bci: 0,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: "obsolete/VmProbe.value:()I".to_string(),
            bci: 0,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    }
}


fn define(shared: &SharedVm, name: &str, constant: i32) -> Option<ClassId> {
    shared
        .classes
        .class_manager
        .write()
        .define_class_with_options(
            name,
            &probe_class(name, constant),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .ok()
}

fn scope(key: &str, semantics: ResumeSemantics) -> cratonvm_jit::deopt::ReconstructedFrame {
    cratonvm_jit::deopt::ReconstructedFrame {
        class_id: 0,
        method_key: key.to_string(),
        bci: 0,
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: Vec::new(),
        semantics,
        caller_frames: Vec::new(),
    }
}


/// A chain whose trapping body is no longer published is vouched for while
/// none of the classes it names was redefined: a redefinition of some other
/// class does not refuse it, one of its own scopes' does.
#[test]
fn a_chain_is_vouched_for_unless_one_of_its_own_classes_was_redefined() {
    const OUTER: &str = "obsolete/VmProbe.value:()I";
    const INNER: &str = "obsolete/InnerProbe.value:()I";
    let shared = SharedVm::new(VmConfig::default());
    let (Some(outer), Some(inner), Some(other)) = (
        define(&shared, "obsolete/VmProbe", 70_000),
        define(&shared, "obsolete/InnerProbe", 5),
        define(&shared, "obsolete/OtherProbe", 9),
    ) else {
        return;
    };
    let (Ok(outer_method), Ok(_)) = (
        resolve_inlined_callee(&shared, outer, OUTER),
        resolve_inlined_callee(&shared, outer, INNER),
    ) else {
        // The stripped test VM does not resolve across the two classes.
        return;
    };
    let mut chain = scope(INNER, ResumeSemantics::REEXECUTE);
    chain.caller_frames = vec![scope(OUTER, ResumeSemantics::for_caller_scope())];
    assert!(chain_scope_classes_never_redefined(
        &shared,
        &outer_method,
        &chain
    ));

    let redefine = |class: ClassId, name: &str| {
        shared
            .classes
            .class_manager
            .write()
            .redefine_class(class, probe_class(name, 6), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
    };
    redefine(other, "obsolete/OtherProbe");
    assert!(
        chain_scope_classes_never_redefined(&shared, &outer_method, &chain),
        "another class's redefinition says nothing about this chain"
    );
    redefine(inner, "obsolete/InnerProbe");
    assert!(
        !chain_scope_classes_never_redefined(&shared, &outer_method, &chain),
        "a scope's own class was redefined"
    );
}


/// Round 14 wave 4 (lane resume2, CH3W-3; patch
/// `r14w3-chain-tierup-sink-inner-own-source-patch`): a chain built with an
/// own-source template records that frame, and the push restamps it with
/// the chain's stamp -- the sinks that run the chain inside the push (the
/// tier-up sink, the call-site service) cannot restamp positionally after
/// it as the doors do. The frame then runs the spliced bytecode against
/// the pool it was compiled at (the OLD constant 5); unrestamped it read
/// the redefined class's pool (6).
#[test]
fn a_pushed_chain_restamps_its_own_source_inner_frames() {
    const INNER: &str = "obsolete/InnerProbe.value:()I";
    let shared = SharedVm::new(VmConfig::default());
    let (Some(outer), Some(inner)) = (
        define(&shared, "obsolete/VmProbe", 70_000),
        define(&shared, "obsolete/InnerProbe", 5),
    ) else {
        return;
    };
    if resolve_inlined_callee(&shared, outer, INNER).is_err() {
        // The stripped test VM does not resolve across the two classes.
        return;
    }
    // The compiled method: `invokestatic #1; ireturn`, parked after the
    // invoke, which hands back whatever the inner frame returns.
    let outermost = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: outer,
            class_name: Arc::from("obsolete/VmProbe"),
            method_name: Arc::from("value"),
            method_descriptor: Arc::from("()I"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(&[0xb8, 0x00, 0x01, 0xac]),
            exception_table: Arc::from(
                Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new(),
            ),
            max_stack: 2,
            max_locals: 0,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ));
    let spliced: Arc<[u8]> = crate::runtime::frame::padded_bytecode(&[0x12, 8, 0xac]);
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("a code buffer");
    buf.emit(&[0xC3]); // ret
    let mut body = cratonvm_jit::CompiledMethod::new(buf);
    body.install_epoch = cratonvm_jit::jit_install_epoch();
    let Some(stamp) = body.compile_cp_stamp() else {
        return;
    };
    body.splice_scope_sources = vec![cratonvm_jit::SpliceScopeSource {
        method_key: INNER.to_string(),
        class_id: inner.as_u32(),
        code: Arc::clone(&spliced),
        max_locals: 0,
        is_static: true,
        exception_table: Vec::new(),
    }];
    let mut chain = scope(INNER, ResumeSemantics::REEXECUTE);
    chain.caller_frames = vec![scope(
        "obsolete/VmProbe.value:()I",
        ResumeSemantics::for_caller_scope(),
    )];
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(
            inner,
            probe_class("obsolete/InnerProbe", 6),
            RedefineOptions::default(),
        )
        .expect("a same-shape redefinition succeeds");
    let _ = shared
        .jit
        .jit_cache
        .invalidate_for_redefinition(inner, "obsolete/InnerProbe");
    let rows = chain_inner_scope_own_sources(&shared, &body, outer, &chain)
        .expect("the redefined callee's spliced body");

    let built = build_deopt_frame_chain_sourced(&shared, &outermost, &chain, &[], &rows)
        .expect("the chain builds from the template");
    assert_eq!(
        built.inner_own_source_frames,
        vec![1],
        "frame 1 is the template's"
    );
    let plain = build_deopt_frame_chain_sourced(&shared, &outermost, &chain, &[], &[])
        .expect("the chain builds from the current body");
    assert!(plain.inner_own_source_frames.is_empty());
    drop(plain);

    let run = |mut c: DeoptFrameChain, restamp_inner: bool| {
        if !restamp_inner {
            c.inner_own_source_frames.clear();
        }
        let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(4_143), "r14w4");
        let r = run_deopt_frame_chain_to_completion(
            &shared,
            &mut thread,
            c.with_outermost_cp_stamp(Some(stamp)),
        );
        assert_eq!(thread.frames.len(), 0, "every pushed frame is popped");
        r
    };
    let restamped = run(built, true).expect("pushed");
    assert!(
        matches!(restamped, Ok(Some(Value::Int(5)))),
        "the spliced body reads the pool it was compiled at, got {restamped:?}"
    );
    let again = build_deopt_frame_chain_sourced(&shared, &outermost, &chain, &[], &rows)
        .expect("the chain builds from the template");
    let unstamped = run(again, false).expect("pushed");
    assert!(
        matches!(unstamped, Ok(Some(Value::Int(6)))),
        "the pre-fix answer reads the redefined pool, got {unstamped:?}"
    );
}

#[test]
fn scoped_class_id_bypasses_ambiguous_splice_hint() {
    let shared = SharedVm::new(VmConfig::default());
    let Some(cid) = define(&shared, "obsolete/AmbigProbe", 7) else {
        return;
    };
    let key = "obsolete/AmbigProbe.value:()I";
    let hints = vec![(key.to_string(), cratonvm_jit::deopt::SPLICE_SCOPE_CLASS_AMBIGUOUS)];
    let err = resolve_inlined_callee_scoped(&shared, cid, key, 0, &hints)
        .err()
        .expect("unscoped ambiguous hint must refuse");
    assert!(err.contains("names two different classes"), "{err}");
    let resolved = resolve_inlined_callee_scoped(&shared, cid, key, cid.as_u32(), &hints)
        .unwrap_or_else(|e| panic!("scoped class_id failed: {e}"));
    assert_eq!(resolved.declaring_class_id, cid);
}
