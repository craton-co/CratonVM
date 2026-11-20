//! Round 13 wave 5 (lane ffm3): `ArenaImpl.allocateNoInit` -- the method
//! every `SegmentAllocator.allocateFrom(layout, value)` default reaches for
//! an `ArenaImpl` -- answers this VM's carrier, not a JDK segment whose
//! `ResourceCleanup` the close walk would run for a shared arena.
use super::*;
use crate::phases_late::foreign_ffm::{p67_arena_slots, p67_memory_session};
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Stands in for the JDK's `allocateNoInit` bytecode.
fn jdk_body(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    (method == "allocateNoInit" && descriptor == ARENA_ALLOCATE_NO_INIT_DESC && args.len() == 2)
        .then_some(Ok(Some(Value::Long(42))))
}

fn obj(o: ObjectRef) -> Value {
    Value::Object(Some(o))
}

/// A shared, closeable, open arena of the shape `foreign_ffm` mints.
fn shared_arena(ctx: &mut MockNativeContext) -> (ObjectRef, ObjectRef) {
    let arena = match try_alloc_concurrent_synthetic(ctx, "jdk/internal/foreign/ArenaImpl", 4) {
        Ok(arena) => arena,
        Err(err) => panic!("no arena: {err:?}"),
    };
    let session = match p67_memory_session(ctx) {
        Ok(Value::Object(Some(session))) => session,
        other => panic!("no session: {other:?}"),
    };
    let slots = p67_arena_slots(&*ctx, arena);
    ctx.set_field(arena, slots.open, Value::Int(1));
    ctx.set_field(arena, slots.session, obj(session));
    ctx.set_field(arena, slots.closeable, Value::Int(1));
    (arena, session)
}

#[test]
fn the_triple_is_a_bridge() {
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    super::register_pe_memory_segment(&mut registry);
    // SBR4 lane segacc retired this triple (`RETIRED_SHADOW_SBR4_SEGACC_TRIPLES`):
    // the registry retags a retired `Bridge` as the `SyntheticStub` it is
    // dropped as under `--real-jdk`, and keeps it live under `--compatible`.
    assert_eq!(
        registry.kind_of(
            "jdk/internal/foreign/ArenaImpl",
            "allocateNoInit",
            ARENA_ALLOCATE_NO_INIT_DESC
        ),
        Some(NativeKind::SyntheticStub)
    );
}

/// A modelled arena gets the carrier, scoped to the arena, and no JDK
/// cleanup is registered -- the JDK body would have registered one that the
/// close walk runs regardless. Round 15 wave 3 (lane ffm2): a SHARED
/// arena's session now lists the block in the model's own block record
/// (`CRATONVM_FFM_SHARED_CLOSE_DEFERRED_FREE`, default on), exactly as a
/// confined one has since round 12 wave 5; its close hands the id to the
/// deferred free instead of freeing it. Nothing else is in the list.
#[test]
fn a_modelled_shared_arena_gets_the_carrier_and_only_a_block_record() {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(jdk_body);
    let (arena, session) = shared_arena(&mut ctx);
    let args = [obj(arena), Value::Long(8), Value::Long(8)];
    let seg = match pe_arena_allocate_no_init(&mut ctx, &args) {
        Ok(Some(Value::Object(Some(seg)))) => seg,
        other => panic!("allocateNoInit gave {other:?}"),
    };
    assert_eq!(ctx.get_field(seg, 1), Value::Long(8));
    // The SESSION, where `AbstractMemorySegmentImpl.scope` sits: the JDK's
    // final `sessionImpl()` reads this slot (`pe_carrier_scope`).
    assert_eq!(ctx.get_field(seg, PE_SEGMENT_ARENA_FIELD), obj(session));
    assert_eq!(pe_segment_session(&ctx, seg), Some(session));
    assert_ne!(ctx.get_field(seg, 0), Value::Long(0), "a real block");
    let actions = crate::phases_late::foreign_ffm::p67_session_actions(&ctx, session);
    assert_eq!(
        actions.len(),
        1,
        "one entry, and no JDK cleanup: {actions:?}"
    );
    let record = actions[0];
    assert!(
        ctx.object_is_array(record)
            && ctx.heap_element_type_of(record) == cratonvm_types::ArrayElementType::Long,
        "the model's `long[]` block record"
    );
    assert_eq!(
        ctx.get_array_element(record, 0),
        Value::Long(1),
        "one block listed"
    );
}

/// The JDK's own argument checks still apply (they are `allocate`'s).
#[test]
fn a_bad_alignment_is_refused_as_the_jdk_refuses_it() {
    let mut ctx = mock_ctx();
    let (arena, _session) = shared_arena(&mut ctx);
    let args = [obj(arena), Value::Long(8), Value::Long(3)];
    let err = match pe_arena_allocate_no_init(&mut ctx, &args) {
        Ok(v) => panic!("alignment 3 was accepted: {v:?}"),
        Err(err) => err,
    };
    assert!(
        format!("{err:?}").contains("Invalid alignment constraint"),
        "{err:?}"
    );
}

/// A receiver this model did not build runs the JDK body.
#[test]
fn a_receiver_this_model_did_not_build_runs_the_jdk_body() {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(jdk_body);
    let other = ctx.alloc_object(cratonvm_types::ClassId::new(0), 2);
    let args = [obj(other), Value::Long(8), Value::Long(8)];
    assert_eq!(
        pe_arena_allocate_no_init(&mut ctx, &args).ok(),
        Some(Some(Value::Long(42)))
    );
}
