use super::*;

/// The probe's `msg` lines: the bare shape the VM funnel completes.
#[test]
fn a_primitive_array_cast_is_worded_as_checkcast() {
    assert_eq!(
        vh_checkcast_message("[J", "[I"),
        "class [J cannot be cast to class [I"
    );
    assert_eq!(
        vh_checkcast_message("[B", "Ljava/nio/ByteBuffer;"),
        "class [B cannot be cast to class java.nio.ByteBuffer"
    );
}

/// HotSpot 25 (`R15Vh6CastMessages`): `checkcast` for a primitive array
/// handle's and a view's coordinate; `Class.cast` for a reference array
/// handle (`String[]`, a user class, `int[][]`).
#[test]
fn only_primitive_array_and_view_coordinates_take_the_checkcast_wording() {
    for yes in ["[I", "[J", "[B", "[Z", "[D", "Ljava/nio/ByteBuffer;"] {
        assert!(vh_coordinate_cast_is_checkcast(yes), "{yes}");
    }
    for no in [
        "[[I",
        "[Ljava/lang/String;",
        "[Ljava/lang/Object;",
        "[LFoo;",
        "Ljava/lang/String;",
        "I",
        "[",
        "[X",
    ] {
        assert!(!vh_coordinate_cast_is_checkcast(no), "{no}");
    }
}
