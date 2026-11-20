use super::{class_get_name_of, no_such_method_message, reflect_text_class_name};
use crate::test_utils::mock_ctx;
use cratonvm_native_api::{NativeClassAccess, NativeContext, NativeHeapAccess};

/// Failed before: `Field.set` on a hidden class's `static final String
/// TAG` printed `... field R21Fz3HiddenAccess1Payload.0x0000000000000002.TAG
/// to java.lang.String` (probe `R21Fz3HiddenAccess1`, fact
/// `outer.setAccessibleFinal`); HotSpot 25 keeps the `/0x` tail.
#[test]
fn a_hidden_class_keeps_its_slash_0x_tail_in_reflection_texts() {
    let mut ctx = mock_ctx();
    let hidden = "p/R21Fz3HiddenAccess1Payload/0x0000000000000002";
    let cid = ctx.declare_loaded_class(hidden);
    // Wave 26, lane hexname: marked hidden as every real hidden define is
    // (`options.hidden` / `set_class_hidden`); since wave 25
    // (`CRATONVM_CLASS_NAME_HIDDEN_BY_CLASS`) the tail of a class the
    // store calls ORDINARY is dotted, as HotSpot dots it.
    ctx.set_class_hidden(cid);
    assert_eq!(
        reflect_text_class_name(&ctx, hidden),
        "p.R21Fz3HiddenAccess1Payload/0x0000000000000002"
    );
    // The field door's qualified name is `<getName()>.<field>`.
    assert_eq!(
        format!("{}.TAG", reflect_text_class_name(&ctx, hidden)),
        "p.R21Fz3HiddenAccess1Payload/0x0000000000000002.TAG"
    );
    // `o.getClass().getName()` of a hidden instance (`getSetMessage`'s
    // attempted type, the invoke0 receiver text).
    let obj = ctx.alloc_object(cid, 0);
    assert_eq!(
        class_get_name_of(&ctx, obj),
        "p.R21Fz3HiddenAccess1Payload/0x0000000000000002"
    );
    // `Class.methodToString`'s owner.
    assert_eq!(
        no_such_method_message(&ctx, cid, "m", None),
        "p.R21Fz3HiddenAccess1Payload/0x0000000000000002.m()"
    );
    // An ordinary class is unchanged.
    assert_eq!(reflect_text_class_name(&ctx, "p/Outer$Inner"), "p.Outer$Inner");
    // The method-chain spellings the other message sites use (an array of
    // a hidden class keeps the tail inside its descriptor).
    use super::ClassTextName;
    assert_eq!(hidden.class_text(&ctx), "p.R21Fz3HiddenAccess1Payload/0x0000000000000002");
    assert_eq!("[Lp/H/0x2a;".hidden_dotted(), "[Lp.H/0x2a;");
    assert_eq!("[Ljava/lang/String;".hidden_dotted(), "[Ljava.lang.String;");
}
