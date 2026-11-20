use super::*;
use std::sync::Arc;

/// `static int get(r9w5/Vol o) { return o.v; }` —
/// `aload_0; getfield #2; ireturn`, padded like a VM method.
fn getter() -> CachedBytecodeMethod {
    CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("r9w5/Vol"),
        method_name: Arc::from("get"),
        method_descriptor: Arc::from("(Lr9w5/Vol;)I"),
        source_file: None,
        code: Arc::from([0x2a, 0xb4, 0x00, 0x02, 0xac, 0x00, 0x00].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 2,
        max_locals: 1,
        num_params: 1,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
    }
}

/// An optimizing (C2) compile of a volatile-field reader comes out of the
/// IR pipeline by default (round 9 wave 7 flip of
/// `CRATONVM_JIT_IR_VOLATILE_FIELDS`): the builder emits an ordered
/// `Op::Load` (`ir::Node::volatile_access`) that GVN / LICM /
/// redundant-load elimination refuse to merge or hoist. The identical
/// non-volatile reader is the control. `CRATONVM_JIT_IR_VOLATILE_FIELDS=0`
/// restores the old route: the volatile reader is refused by the IR builder
/// and compiles single-pass, which hoists no field read.
#[test]
// Routing assertion: on aarch64 `try_compile_inner` returns before the
// x86-64 tier choice, like the other routing tests.
#[cfg(target_arch = "x86_64")]
fn a_volatile_getfield_takes_the_ir_tier_by_default() {
    // Routing, not the acceptance policy: publish every body that lowers,
    // so the control's trivial getter is not refused for want of evidence.
    let _accept = crate::ir_evidence::AcceptAlways::on();
    // The optimizing tier is gated off while the young generation can
    // relocate; pin it off on this thread so the control can reach it.
    crate::x64::set_moving_young_override(Some(false));
    let cached = getter();
    // SAFETY: an all-zero `JitRuntimeHelpers` is valid (every entry is an
    // address or a flag); the two entries the field lowering needs are
    // non-zero so the IR lowerer does not refuse for want of a helper.
    // The generated code is never executed.
    let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    helpers.getfield = 1;
    helpers.putfield_int = 1;
    let field = |cp: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
        (cp == 2).then_some((0, b'I', None))
    };
    let compile = |volatile: bool| -> (bool, u64) {
        let is_volatile = move |cp: u16| volatile && cp == 2;
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let compiled = try_compile_request(&CompileRequest {
            cp_field_resolver: Some(&field),
            cp_field_volatile_resolver: Some(&is_volatile),
            optimize: true,
            ..CompileRequest::new(&cached, &helpers, CompileRealm::process())
        });
        (compiled.is_some(), IR_LOWER_COMPILES.with(|c| c.get()))
    };
    assert_eq!(
        compile(false),
        (true, 1),
        "control: a plain int field reader must compile through the IR pipeline"
    );
    assert_eq!(
        compile(true),
        (true, 1),
        "a volatile field reader now compiles through the IR pipeline (ordered Op::Load)"
    );
    crate::x64::set_moving_young_override(None);
}
