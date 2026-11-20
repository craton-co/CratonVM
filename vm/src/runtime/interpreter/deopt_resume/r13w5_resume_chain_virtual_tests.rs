use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;
use cratonvm_jit::deopt::{FrameValue, MonitorInfo, ReconstructedFrame, VirtualObjectState};

fn cached(method_name: &str, is_synchronized: bool) -> Arc<CachedBytecodeMethod> {
    Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: cratonvm_types::ClassId::new(0),
            class_name: Arc::from("T"),
            method_name: Arc::from(method_name),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: Arc::from(&[0xb1u8][..]), // return
            exception_table: Arc::from(Vec::new().into_boxed_slice()),
            max_stack: 8,
            max_locals: 4,
            num_params: 0,
            is_synchronized,
            is_static: true,
        },
    ))
}

fn vobj(id: usize, class_id: u32, fields: Vec<FrameValue>) -> FrameValue {
    FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id,
        class_id,
        num_fields: fields.len(),
        field_values: fields,
    })
}

fn scope(key: &str, locals: Vec<FrameValue>, stack: Vec<FrameValue>) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: key.to_string(),
        locals,
        stack,
        semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
        ..Default::default()
    }
}

/// `T.outer` defines object 4 in local 0; the spliced `T.inner` names it
/// by reference in its local 0 and holds an int on its stack.
fn shared_object_chain(inner_ref: usize) -> ReconstructedFrame {
    let outer = scope(
        "T.outer:()V",
        vec![vobj(4, 5, vec![FrameValue::Int(9)]), FrameValue::Int(1)],
        Vec::new(),
    );
    let mut rf = scope(
        "T.inner:()V",
        vec![FrameValue::VirtualObjectRef(inner_ref)],
        vec![FrameValue::Int(3)],
    );
    rf.caller_frames = vec![outer];
    rf
}

/// The frames `materialise_inlined_chain` would have built over the
/// placeholders of [`shared_object_chain`].
/// [`cached`] whose body reads local 0 first (`aload_0; pop; return`), so
/// the slot is live at the resume pc 0. gci i4/il: a resumed static frame
/// now filters slot 0 by liveness like any other local, and the plain
/// `return` body left the materialised object dead.
fn cached_reading_local0(method_name: &str) -> Arc<CachedBytecodeMethod> {
    let c = cached(method_name, false);
    Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: c.declaring_class_id,
            class_name: c.class_name.clone(),
            method_name: c.method_name.clone(),
            method_descriptor: c.method_descriptor.clone(),
            source_file: None,
            code: Arc::from(&[0x2au8, 0x57, 0xb1][..]), // aload_0; pop; return
            exception_table: Arc::from(Vec::new().into_boxed_slice()),
            max_stack: 8,
            max_locals: 4,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ))
}

fn placeholder_frames() -> Vec<InlinedChainFrame> {
    vec![
        InlinedChainFrame {
            cached: cached_reading_local0("outer"),
            locals: vec![Value::Object(None), Value::Int(1)],
            stack: Vec::new(),
            resume_pc: 0,
            executing_pc: 0,
            monitors: Vec::new(),
        },
        InlinedChainFrame {
            cached: cached_reading_local0("inner"),
            locals: vec![Value::Object(None)],
            stack: vec![Value::Int(3)],
            resume_pc: 0,
            executing_pc: 0,
            monitors: Vec::new(),
        },
    ]
}

#[test]
fn one_object_named_by_two_scopes_becomes_one_heap_object() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let rf = shared_object_chain(4);
    assert!(reconstructed_chain_names_virtual(&rf));
    let virtuals = merge_chain_scopes(&rf);
    assert_eq!(
        virtuals.shapes,
        vec![(2, 0), (1, 1)],
        "outermost scope first"
    );
    let chain = DeoptFrameChain {
        frames: placeholder_frames(),
        virtuals: Some(virtuals),
        outermost_cp_stamp: None,
        inner_own_source_frames: Vec::new(),
        forced: false,
    };
    let pins = thread.native_pin_roots.len();
    let base = thread.frames.len();
    assert!(matches!(
        push_inlined_chain(&shared, &mut thread, chain, false),
        Some(CachedCallResult::FramePushed)
    ));
    assert_eq!(
        thread.native_pin_roots.len(),
        pins,
        "the pins end at the push"
    );
    // The pushed frames alone keep the object alive across a collection.
    maybe_gc_forced_pub_at(&shared, &mut thread, "deopt-resume");
    let (outer_obj, inner_obj) = match (
        thread.frames[base].get_local(0),
        thread.frames[base + 1].get_local(0),
    ) {
        (Value::Object(Some(o)), Value::Object(Some(i))) => (o, i),
        other => panic!("both scopes must hold the materialised object, got {other:?}"),
    };
    assert_eq!(outer_obj, inner_obj, "one object, not one per scope");
    assert_eq!(
        shared.mem.heap.class_id_of(outer_obj),
        cratonvm_types::ClassId::new(5)
    );
    assert_eq!(shared.mem.heap.get_field(outer_obj, 0), Value::Int(9));
    assert_eq!(thread.frames[base].get_local(1), Value::Int(1));
    assert_eq!(thread.frames[base + 1].stack.len(), 1);
}

/// A graph the materialiser refuses (a reference to an object no scope
/// defines) pushes NOTHING and leaves no pin behind.
#[test]
fn an_unmaterialisable_chain_graph_pushes_nothing() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let rf = shared_object_chain(6);
    let mut chain = DeoptFrameChain {
        frames: placeholder_frames(),
        virtuals: Some(merge_chain_scopes(&rf)),
        outermost_cp_stamp: None,
        inner_own_source_frames: Vec::new(),
        forced: false,
    };
    let pins = thread.native_pin_roots.len();
    let base = thread.frames.len();
    assert!(matches!(
        materialise_chain_virtuals(&shared, &mut thread, &mut chain),
        Err(DeoptFrameBail::VirtualMaterialise)
    ));
    assert_eq!(thread.native_pin_roots.len(), pins);
    assert!(push_inlined_chain(&shared, &mut thread, chain, false).is_none());
    assert_eq!(thread.frames.len(), base, "nothing pushed");
    assert_eq!(thread.native_pin_roots.len(), pins);
}

#[test]
fn placeholders_blank_every_scope_and_only_virtual_slots() {
    let rf = shared_object_chain(4);
    let blank = with_virtual_placeholders(&rf);
    assert!(!reconstructed_chain_names_virtual(&blank));
    assert!(matches!(blank.locals[0], FrameValue::Object(0)));
    assert!(matches!(blank.stack[0], FrameValue::Int(3)));
    assert!(matches!(
        blank.caller_frames[0].locals[0],
        FrameValue::Object(0)
    ));
    assert!(matches!(
        blank.caller_frames[0].locals[1],
        FrameValue::Int(1)
    ));
}

/// `CRATONVM_DEOPT_VERIFY`'s reference check reaches every CALLER scope
/// of a chain, not only the trapping one.
#[test]
fn the_chain_oop_check_reads_every_caller_scope() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let obj = shared
        .mem
        .heap
        .alloc_object(cratonvm_types::ClassId::new(7), 0);
    // Cast: object pointer to the raw word a stash carries.
    let addr = obj.as_ptr() as usize as u64;
    let mut rf = scope("T.inner:()V", vec![FrameValue::Object(addr)], Vec::new());
    rf.caller_frames = vec![scope(
        "T.outer:()V",
        vec![FrameValue::Object(addr), FrameValue::Object(0)],
        Vec::new(),
    )];
    assert!(verify_chain_oops(&rf, &shared).is_ok());
    rf.caller_frames[0].locals[1] = FrameValue::Object(0x1234);
    assert!(
        verify_reconstructed_oops(&rf, &shared).is_ok(),
        "the old check missed it"
    );
    let why = verify_chain_oops(&rf, &shared).expect_err("a wild caller word is caught");
    assert!(why.contains("caller scope 1"), "{why}");
}

#[test]
fn a_monitor_or_a_synchronized_outer_method_refuses_the_chain_virtuals() {
    let rf = shared_object_chain(4);
    assert_eq!(chain_virtuals_refusal(&cached("outer", false), &rf), None);
    assert!(chain_virtuals_refusal(&cached("outer", true), &rf).is_some());
    let mut locked = rf.clone();
    locked.caller_frames[0].monitors.push(MonitorInfo {
        object: FrameValue::Object(0x1000),
        lock_depth: 1,
        relock: false,
    });
    assert!(chain_virtuals_refusal(&cached("outer", false), &locked).is_some());
}
