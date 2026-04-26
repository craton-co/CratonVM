// Re-export from rustjvm-types crate.
pub use rustjvm_types::{
    decode_value, encode_value, is_object_tag, CompactTag, CompactValue, ObjectRef, Value,
    VTAG_DOUBLE, VTAG_FLOAT, VTAG_INT, VTAG_LONG, VTAG_NULL, VTAG_OBJECT, VTAG_RETADDR,
    VTAG_UNINIT,
};
