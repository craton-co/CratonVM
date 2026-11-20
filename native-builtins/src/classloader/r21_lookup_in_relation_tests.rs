use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
use cratonvm_types::ClassId;

fn class(ctx: &mut MockNativeContext, name: &str) -> (ClassId, Value) {
    let id = ctx.ensure_class_initialized(name).unwrap();
    let mirror = ctx.get_class_mirror(id);
    (id, Value::Object(Some(mirror)))
}

fn rel(ctx: &mut MockNativeContext, a: Value, b: Value) -> (bool, bool, bool) {
    lk_class_relation(ctx, a, b, false).unwrap()
}

fn member(ctx: &MockNativeContext, id: ClassId, inner: &str, outer: &str, simple: &str) {
    let entry = (inner.to_string(), outer.to_string(), simple.to_string(), 0);
    ctx.set_inner_classes(id, vec![entry]);
}

/// `lookup().in(p.Host.class)` from a lambda's hidden class keeps PACKAGE
/// (getPackageName cuts `p.Host$$Lambda/0x1a` at its last `.`) and loses
/// PRIVATE (a hidden class has no InnerClasses link to `Host`): 95 -> 25.
/// The old split put the hidden class in package `p/Host$$Lambda` (-> 1).
#[test]
fn hidden_class_is_in_its_hosts_package_but_not_its_nest() {
    let mut ctx = MockNativeContext::new();
    let (_, host) = class(&mut ctx, "p/Host");
    let (hid, hidden) = class(&mut ctx, "p/Host$$Lambda/0x1a");
    ctx.set_class_hidden(hid);
    assert_eq!(rel(&mut ctx, hidden, host), (false, true, false));
    assert_eq!(rel(&mut ctx, host, hidden), (false, true, false));
    assert_eq!(lk_in_modes(LK_FULL_POWER, false, true, false, true), 25);
}

/// A NESTMATE hidden class shares its host's nest, but `Lookup.in` reads
/// `getEnclosingClass`, never `getNestHost`: still not a package member.
#[test]
fn nestmate_hidden_class_still_is_its_own_outermost_class() {
    let mut ctx = MockNativeContext::new();
    let (_, host) = class(&mut ctx, "p/Host");
    let (hid, hidden) = class(&mut ctx, "p/Host/0x2b");
    ctx.set_class_hidden(hid);
    ctx.set_nest_host_override(hid, "p/Host");
    assert_eq!(rel(&mut ctx, hidden, host), (false, true, false));
}

/// A member class (its own InnerClasses entry) is a package member of its
/// top-level class, through any depth; a sibling top-level class is not.
#[test]
fn member_classes_share_the_outermost_class() {
    let mut ctx = MockNativeContext::new();
    let (_, a) = class(&mut ctx, "p/A");
    let (bid, _) = class(&mut ctx, "p/A$B");
    member(&ctx, bid, "p/A$B", "p/A", "B");
    let (cid, c) = class(&mut ctx, "p/A$B$C");
    member(&ctx, cid, "p/A$B$C", "p/A$B", "C");
    let (did, d) = class(&mut ctx, "p/A$D");
    member(&ctx, did, "p/A$D", "p/A", "D");
    let (_, other) = class(&mut ctx, "p/Other");
    assert_eq!(rel(&mut ctx, c, a), (false, true, true));
    assert_eq!(rel(&mut ctx, c, d), (false, true, true));
    assert_eq!(rel(&mut ctx, a, other), (false, true, false));
    assert_eq!(rel(&mut ctx, a, a), (true, true, true));
}

/// A top-level class whose name holds a `$` is not nested in the class
/// named by its prefix (the old `$` guess said it was).
#[test]
fn dollar_in_a_top_level_name_is_not_nesting() {
    let mut ctx = MockNativeContext::new();
    let (_, odd) = class(&mut ctx, "p/Odd$Name");
    let (_, base) = class(&mut ctx, "p/Odd");
    assert_eq!(rel(&mut ctx, odd, base), (false, true, false));
}

/// An enclosing class that is not loaded yet is compared by name (no load,
/// no GC point); two members of it are still one nest.
#[test]
fn unloaded_outer_class_compares_by_name() {
    let mut ctx = MockNativeContext::new();
    let (yid, y) = class(&mut ctx, "p/X$Y");
    member(&ctx, yid, "p/X$Y", "p/X", "Y");
    let (zid, z) = class(&mut ctx, "p/X$Z");
    member(&ctx, zid, "p/X$Z", "p/X", "Z");
    assert_eq!(rel(&mut ctx, y, z), (false, true, true));
    assert!(ctx.class_init_requests().iter().all(|n| n != "p/X"));
}

/// A hidden class defined from `p/Outer$Mem` bytes keeps an `InnerClasses`
/// self entry under its mangled name (the define rewrites it), but
/// `getDeclaringClass0` refuses that link on HotSpot: it is its own
/// outermost class, so `in()` with `p/Outer` drops PRIVATE both ways.
#[test]
fn hidden_member_bytes_do_not_reach_the_outer_class() {
    let mut ctx = MockNativeContext::new();
    let (_, outer) = class(&mut ctx, "p/Outer");
    let (mid, mem) = class(&mut ctx, "p/Outer$Mem");
    member(&ctx, mid, "p/Outer$Mem", "p/Outer", "Mem");
    let (hid, hidden) = class(&mut ctx, "p/Outer$Mem/0x3c");
    member(&ctx, hid, "p/Outer$Mem/0x3c", "p/Outer", "Mem");
    ctx.set_class_hidden(hid);
    assert_eq!(rel(&mut ctx, hidden, outer), (false, true, false));
    assert_eq!(rel(&mut ctx, outer, hidden), (false, true, false));
    assert_eq!(rel(&mut ctx, hidden, mem), (false, true, false));
    // The non-hidden member class still reaches its outer class.
    assert_eq!(rel(&mut ctx, mem, outer), (false, true, true));
}

/// Wave 12, measured on HotSpot 25: while PRIVATE survives, `in()` both
/// ways between that hidden class and its outer class throws
/// `getDeclaringClass0`'s IncompatibleClassChangeError. Another package,
/// or no PRIVATE, does not consult the chain.
#[test]
fn hidden_member_bytes_raise_the_declaring_class_icce_when_private_survives() {
    let mut ctx = MockNativeContext::new();
    let (_, outer) = class(&mut ctx, "p/Outer");
    let (hid, hidden) = class(&mut ctx, "p/Outer$Mem/0x3c");
    member(&ctx, hid, "p/Outer$Mem/0x3c", "p/Outer", "Mem");
    ctx.set_class_hidden(hid);
    let (_, other) = class(&mut ctx, "q/Other");
    let expected = "p.Outer and p.Outer$Mem/0x3c disagree on InnerClasses attribute";
    for (a, b) in [(hidden, outer), (outer, hidden)] {
        match lk_class_relation(&mut ctx, a, b, true) {
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(
                cratonvm_types::error::LinkageError::IncompatibleClassChangeError { message },
            ))) => assert_eq!(message, expected),
            other => panic!("expected the ICCE, got {other:?}"),
        }
    }
    assert_eq!(rel(&mut ctx, hidden, outer), (false, true, false));
    let across = lk_class_relation(&mut ctx, hidden, other, true).unwrap();
    assert_eq!(across, (false, false, false));
}

/// JIT round 21 wave 14, lane innerclasses
/// (`CRATONVM_CLASS_INNER_CLASSES_SPOOF_CHECK`; probe
/// `R21LookupInSpoofedMember`, HotSpot 25): an ORDINARY class that claims
/// membership in an outer class whose `InnerClasses` does not list it.
/// While PRIVATE survives, `in()` both ways throws `getDeclaringClass0`'s
/// ICCE; without PRIVATE the chain is not consulted; a consistent pair
/// is one nest.
#[test]
fn spoofed_ordinary_member_raises_the_icce_when_private_survives() {
    if !crate::lang_class::class_inner_classes_check_enabled()
    {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let (_, o1) = class(&mut ctx, "r21spoof/O1");
    let (gid, g1) = class(&mut ctx, "r21spoof/G1");
    member(&ctx, gid, "r21spoof/G1", "r21spoof/O1", "G1");
    let expected = "r21spoof.O1 and r21spoof.G1 disagree on InnerClasses attribute";
    for (a, b) in [(g1, o1), (o1, g1)] {
        match lk_class_relation(&mut ctx, a, b, true) {
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(
                cratonvm_types::error::LinkageError::IncompatibleClassChangeError { message },
            ))) => assert_eq!(message, expected),
            other => panic!("expected the ICCE, got {other:?}"),
        }
    }
    // PRIVATE dropped: no walk error (the link is followed as before).
    assert_eq!(rel(&mut ctx, g1, o1), (false, true, true));
    // The consistent control: the outer class lists the member.
    let (oid2, o2) = class(&mut ctx, "r21spoof/O2");
    let (mid2, m2) = class(&mut ctx, "r21spoof/M2");
    member(&ctx, mid2, "r21spoof/M2", "r21spoof/O2", "M2");
    member(&ctx, oid2, "r21spoof/M2", "r21spoof/O2", "M2");
    assert_eq!(lk_class_relation(&mut ctx, m2, o2, true).unwrap(), (false, true, true));
    assert_eq!(lk_class_relation(&mut ctx, o2, m2, true).unwrap(), (false, true, true));
}

/// JIT round 21 wave 16, lane lookup-in (`CRATONVM_LOOKUP_IN_LOADS_OUTER`;
/// page `r21-review-w10-findings-FIXED-20261020.md` residual 3R).
fn loads_outer_on() -> bool {
    true
        && crate::lang_class::class_inner_classes_check_enabled()    }

fn expect_icce(result: Result<(bool, bool, bool), MethodCallFailed>, expected: &str) {
    match result {
        Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(
            cratonvm_types::error::LinkageError::IncompatibleClassChangeError { message },
        ))) => assert_eq!(message, expected),
        other => panic!("expected the ICCE, got {other:?}"),
    }
}

/// The hidden class from member bytes whose outer class is NOT loaded
/// yet: the non-loading entry answers PRIVATE-dropped modes without a
/// load; the rooted entry loads the outer class through the hidden
/// class's own resolution and throws HotSpot's ICCE, as it does when the
/// outer class is loaded. The roots come back.
#[test]
fn hidden_member_bytes_load_an_unloaded_outer_class_and_raise_the_icce() {
    if !loads_outer_on() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let (hid, hidden) = class(&mut ctx, "r21lo/Outer$Mem/0x4d");
    member(&ctx, hid, "r21lo/Outer$Mem/0x4d", "r21lo/Outer", "Mem");
    ctx.set_class_hidden(hid);
    let (_, other) = class(&mut ctx, "r21lo/Other");
    assert_eq!(
        lk_class_relation(&mut ctx, hidden, other, true).unwrap(),
        (false, true, false)
    );
    assert!(ctx.class_init_requests().iter().all(|n| n != "r21lo/Outer"));
    let mut roots = [other];
    expect_icce(
        lk_class_relation_rooted(&mut ctx, hidden, other, true, &mut roots),
        "r21lo.Outer and r21lo.Outer$Mem/0x4d disagree on InnerClasses attribute",
    );
    assert!(ctx.class_init_requests().iter().any(|n| n == "r21lo/Outer"));
    assert_eq!(roots[0], other);
}

/// An ordinary class claiming membership in an outer class that is not
/// loaded yet: once loaded, the outer class does not list it, so the
/// rooted entry throws the spoofed-member ICCE, from either side.
#[test]
fn spoofed_member_of_an_unloaded_outer_class_raises_the_icce() {
    if !loads_outer_on()
    {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let (gid, g) = class(&mut ctx, "r21ls/G");
    member(&ctx, gid, "r21ls/G", "r21ls/O", "G");
    let (_, x) = class(&mut ctx, "r21ls/X");
    let expected = "r21ls.O and r21ls.G disagree on InnerClasses attribute";
    expect_icce(lk_class_relation_rooted(&mut ctx, g, x, true, &mut []), expected);
    expect_icce(lk_class_relation_rooted(&mut ctx, x, g, true, &mut []), expected);
}

/// An outer class that does not exist: the rooted entry throws its
/// resolution error (`NoClassDefFoundError`, as `getDeclaringClass0`
/// does). Without PRIVATE the chain is not consulted: no load, no error.
#[test]
fn missing_outer_class_is_the_resolution_error() {
    if !loads_outer_on() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let (mid, m) = class(&mut ctx, "r21lm/Gone$M");
    member(&ctx, mid, "r21lm/Gone$M", "r21lm/Gone", "M");
    let (_, x) = class(&mut ctx, "r21lm/X");
    if !crate::lang_class::outer_resolution_errors_apply(&ctx, mid) {
        return;
    }
    ctx.fail_class_init("r21lm/Gone");
    assert_eq!(
        lk_class_relation_rooted(&mut ctx, m, x, false, &mut []).unwrap(),
        (false, true, false)
    );
    assert!(ctx.class_init_requests().iter().all(|n| n != "r21lm/Gone"));
    match lk_class_relation_rooted(&mut ctx, m, x, true, &mut []) {
        Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(
            cratonvm_types::error::LinkageError::NoClassDefFoundError { class_name },
        ))) => assert_eq!(class_name, "r21lm/Gone"),
        other => panic!("expected the NoClassDefFoundError, got {other:?}"),
    }
}


/// `isSamePackage` compares defining loaders first.
#[test]
fn same_package_name_of_another_loader_is_another_package() {
    let mut ctx = MockNativeContext::new();
    let (_, a) = class(&mut ctx, "p/A");
    let (bid, b) = class(&mut ctx, "p/B");
    ctx.set_loader_id_override(bid, 5);
    assert_eq!(rel(&mut ctx, a, b), (false, false, false));
}
