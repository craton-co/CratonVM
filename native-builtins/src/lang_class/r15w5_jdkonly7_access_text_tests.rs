use super::hotspot_illegal_access_message;
use crate::test_utils::mock_ctx;

/// Failed before JO6-1: the refusal read `cannot access member: modifiers
/// 0x000a, ..`.
#[test]
fn a_member_refusal_reads_as_reflections_text() {
    let mut ctx = mock_ctx();
    let caller = ctx.declare_loaded_class("p/Caller");
    let member = ctx.declare_loaded_class("p/Target");
    ctx.set_class_access_flags(caller, 0x0021);
    ctx.set_class_access_flags(member, 0x0020);
    assert_eq!(
        hotspot_illegal_access_message(&mut ctx, caller, member, 0x0002 | 0x0008),
        "class p.Caller cannot access a member of class p.Target with modifiers \"private static\""
    );
    // No public/protected/private bit: JDK 25 `msgSuffix` reads ` with
    // package access` (t1 wave 2, lane P; was empty, matching no JDK 25 text).
    assert_eq!(
        hotspot_illegal_access_message(&mut ctx, caller, member, 0x0008),
        "class p.Caller cannot access a member of class p.Target with package access"
    );
}
