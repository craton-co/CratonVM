use super::{p59_sw_stream, p59_walk_jdk_stream_enabled, P59_WALK_SPLITERATOR_CHARACTERISTICS};
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

/// HotSpot's walk spliterator reports `ORDERED | IMMUTABLE` (1040), not
/// `SIZED`: `Nio21ConcreteReceivers`' `walk-spliterator` row prints it.
#[test]
fn walk_spliterator_characteristics_are_hotspots() {
    assert_eq!(P59_WALK_SPLITERATOR_CHARACTERISTICS, 1040);
}

/// A context with no `JdkStreams` row (the mock is `--compatible` with
/// no row) keeps the carrier: its stream world is the synthetic one.
#[test]
fn walk_jdk_stream_follows_the_jdk_streams_row() {
    let ctx = mock_ctx();
    assert!(!p59_walk_jdk_stream_enabled(&ctx));
}

/// Whatever the builder answers, it is never a receiver `new` could not
/// produce (JVMS 6.5): either `None` (the caller keeps the carrier) or a
/// stream of a concrete class.
#[test]
fn walk_jdk_stream_is_concrete_or_declined() {
    let mut ctx = mock_ctx();
    let frames = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
    if let Some(built) = p59_sw_stream(&mut ctx, frames) {
        let cid = ctx.class_id_of_object(built.stream);
        assert!(cratonvm_native_api::instantiable::class_is_instantiable(
            &ctx, cid
        ));
    }
}
