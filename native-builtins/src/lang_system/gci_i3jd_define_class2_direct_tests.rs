use super::read_byte_buffer_define_class_slice;
use crate::test_utils::{mock_ctx, MockNativeContext};
use cratonvm_native_api::{FieldMetadata, NativeHeapAccess};
use cratonvm_types::{ClassId, ObjectRef, Value};

/// A class id no other mock object uses, declared with the direct
/// buffer's fields so `bb_define_layout` and `get_field_by_name("address")`
/// resolve them as on a real `DirectByteBuffer`.
const DIRECT_BB: u32 = 0x6A_D3;

fn direct_buffer(
    ctx: &mut MockNativeContext,
    backing: &[u8],
    position: i32,
    limit: i32,
) -> ObjectRef {
    let cid = ClassId::new(DIRECT_BB);
    let field = |name: &str, descriptor: &str, slot: usize| FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: cid,
        is_static: false,
    };
    ctx.set_declared_fields(
        cid,
        vec![
            field("hb", "[B", 0),
            field("position", "I", 1),
            field("limit", "I", 2),
            field("capacity", "I", 3),
            field("address", "J", 4),
        ],
    );
    let bb = ctx.alloc_object(cid, 5);
    // `hb` stays null: a direct buffer has no backing array.
    ctx.set_field(bb, 0, Value::Object(None));
    ctx.set_field(bb, 1, Value::Int(position));
    ctx.set_field(bb, 2, Value::Int(limit));
    ctx.set_field(bb, 3, Value::Int(backing.len() as i32));
    ctx.set_field(bb, 4, Value::Long(backing.as_ptr() as usize as i64));
    bb
}

const CLASS_BYTES: [u8; 6] = [0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x01];

#[test]
fn a_direct_buffer_advanced_past_a_prefix_reads_from_its_position() {
    let mut ctx = mock_ctx();
    let mut backing = vec![0x5Au8; 8];
    backing.extend_from_slice(&CLASS_BYTES);
    let bb = direct_buffer(&mut ctx, &backing, 8, backing.len() as i32);
    // What the JDK passes: offset = b.position(), length = b.remaining().
    let bytes = read_byte_buffer_define_class_slice(&ctx, bb, 8, CLASS_BYTES.len(), "p/X")
        .expect("the class bytes at the buffer's position");
    assert_eq!(bytes, CLASS_BYTES.to_vec());
}

#[test]
fn a_direct_buffer_at_position_zero_reads_from_its_address() {
    let mut ctx = mock_ctx();
    let backing = CLASS_BYTES.to_vec();
    let bb = direct_buffer(&mut ctx, &backing, 0, backing.len() as i32);
    let bytes = read_byte_buffer_define_class_slice(&ctx, bb, 0, CLASS_BYTES.len(), "p/X")
        .expect("the whole buffer");
    assert_eq!(bytes, CLASS_BYTES.to_vec());
}

#[test]
fn a_direct_slice_past_the_limit_is_refused() {
    let mut ctx = mock_ctx();
    let mut backing = vec![0x5Au8; 8];
    backing.extend_from_slice(&CLASS_BYTES);
    // Limit one byte short of the class.
    let bb = direct_buffer(&mut ctx, &backing, 8, backing.len() as i32 - 1);
    let r = read_byte_buffer_define_class_slice(&ctx, bb, 8, CLASS_BYTES.len(), "p/X");
    assert!(r.is_err(), "a slice past the limit must not be read: {r:?}");
}
