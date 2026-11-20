#[allow(unused_imports)]
use super::*;

#[cfg(test)]
mod i101_l4_bound_field_lane_tests {
use super::bound_site_matches;

/// The bound handle's site is the accessor's type with its first parameter
/// dropped, whatever that parameter's spelling (class, array, primitive).
#[test]
fn i101_bound_site_drops_exactly_the_first_parameter() {
    assert!(bound_site_matches("(LBox;)I", "()I"));
    assert!(bound_site_matches("(LBox;I)V", "(I)V"));
    assert!(bound_site_matches("(Lp/Box;Ljava/lang/String;)V", "(Ljava/lang/String;)V"));
    assert!(bound_site_matches("([[LBox;)J", "()J"));
    assert!(bound_site_matches("(IJ)V", "(J)V"));
    assert!(!bound_site_matches("(LBox;)I", "(LBox;)I"));
    assert!(!bound_site_matches("(LBox;)I", "()J"));
    assert!(!bound_site_matches("()I", "()I"));
    assert!(!bound_site_matches("(LBox)I", "()I"));
    assert!(!bound_site_matches("LBox;", "()I"));
}

/// Wave 102: two handles in ONE set, called alternately, both stay
/// recorded (the direct-mapped memo evicted one per call).
#[test]
fn i102_two_handles_sharing_a_set_keep_their_lines() {
    use super::{
        bound_field_memo_file, bound_field_memo_find, bound_field_memo_set, BoundFieldLine,
        BOUND_FIELD_MEMO_WAYS,
    };
    let vm = 0x5eed_usize;
    let a = 0x1000_usize;
    let set = bound_field_memo_set(a);
    let mut same: Vec<usize> = (1..100_000usize)
        .map(|k| a + k * 8)
        .filter(|&x| bound_field_memo_set(x) == set)
        .take(BOUND_FIELD_MEMO_WAYS)
        .collect();
    assert_eq!(same.len(), BOUND_FIELD_MEMO_WAYS);
    let b = same.remove(0);
    let line = |addr| BoundFieldLine {
        vm,
        epoch: 1,
        addr,
        accessor: None,
        base: None,
    };
    bound_field_memo_file(line(a));
    bound_field_memo_file(line(b));
    for _ in 0..8 {
        assert!(bound_field_memo_find(vm, 1, a).is_some());
        assert!(bound_field_memo_find(vm, 1, b).is_some());
    }
    // Another epoch is a miss; filing the same handle again replaces its
    // line instead of taking a second way.
    assert!(bound_field_memo_find(vm, 2, a).is_none());
    bound_field_memo_file(BoundFieldLine { epoch: 2, ..line(a) });
    for &x in &same {
        bound_field_memo_file(line(x));
    }
    // Three newer handles filled the set: `a` (newest of the first two
    // after its refile) is still in, `b` is the evicted LRU line.
    assert!(bound_field_memo_find(vm, 2, a).is_some());
    assert!(bound_field_memo_find(vm, 1, b).is_none());
}
}

/// gci cl48 (page
/// `gci-i47w-jw1-compatible-child-first-loader-statics-read-back-wrong-FIXED-20261017`):
/// a real `DirectMethodHandle$StaticAccessor` resolves its `staticOffset`
/// against its own `staticBase`, so the handles of the child-first copies of
/// one class (one minted offset) each reach their own copy's static.
#[cfg(test)]
mod gci_i48_cl48_static_accessor_tests {
use super::real_static_accessor_target;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess,
};
use cratonvm_types::{ObjectRef, Value};

// An identity no other test uses (mock contexts default to 0).
const VM: usize = 0xC148_0011;
const NAME: &str = "gci48cl48/AccessorHolder";

/// A `StaticAccessor` stand-in whose only field is `staticBase`.
fn accessor(ctx: &mut MockNativeContext, base: Option<ObjectRef>) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized("java/lang/invoke/DirectMethodHandle$StaticAccessor")
        .expect("mock class id");
    ctx.set_declared_fields(
        cid,
        vec![FieldMetadata {
            name: "staticBase".to_string(),
            descriptor: "Ljava/lang/Object;".to_string(),
            access_flags: 0x0010,
            slot_index: 0,
            declaring_class_id: cid,
            is_static: false,
        }],
    );
    let acc = ctx.alloc_object(cid, 1);
    ctx.set_field(acc, 0, Value::Object(base));
    acc
}

#[test]
fn gci_i48_cl48_each_static_accessor_reaches_its_own_copy() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let mut classes = vec![ctx.ensure_class_initialized(NAME).expect("mock class id")];
    for _ in 1..8 {
        classes.push(ctx.declare_second_copy(NAME));
    }
    let offsets: Vec<i64> = classes
        .iter()
        .map(|&c| crate::register_static_field_offset_in_vm(VM, NAME, "graph", c, 0))
        .collect();
    // Since dev's `eab6d094dd` a static offset carries the class id: every
    // copy has its own offset, and each accessor names its own copy.
    let mut distinct = offsets.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), offsets.len(), "one copy, one offset");
    for (k, &c) in classes.iter().enumerate() {
        let offset = offsets[k] as usize;
        let mirror = ctx.get_class_mirror(c);
        let acc = accessor(&mut ctx, Some(mirror));
        assert_eq!(
            real_static_accessor_target(&mut ctx, acc, offset),
            Some((c, 0)),
            "the accessor of copy {} names that copy",
            c.as_u32()
        );
        // No base: the copy registered under that offset.
        let acc = accessor(&mut ctx, None);
        assert_eq!(real_static_accessor_target(&mut ctx, acc, offset), Some((c, 0)));
    }
    crate::forget_vm_unsafe_side_stores(VM);
}
}

/// Interpreter round i6 wave 121, lane L2 (page
/// `i115-L4-proposal-shadowed-jdk-bodies-compile-for-their-bytecode-only-reentries`):
/// the re-entry-only set moved from `vm_init.rs` to its registrar. The four
/// rows are named, each still an `Intrinsic` (marking changes no kind), and
/// a second registration pass adds no duplicate.
#[cfg(test)]
mod i121_l2_reentry_only_bodies_tests {
use super::register_t4_method_handle_invoke;
use cratonvm_native_api::{NativeKind, NativeMethodRegistry};

#[test]
fn t4_registrar_names_the_four_reentry_only_bodies() {
    let mut registry = NativeMethodRegistry::new();
    register_t4_method_handle_invoke(&mut registry);
    register_t4_method_handle_invoke(&mut registry);
    let mh = "java/lang/invoke/MethodHandle";
    let want = [
        (
            mh,
            "asType",
            "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        ),
        (mh, "invokeWithArguments", "([Ljava/lang/Object;)Ljava/lang/Object;"),
        (mh, "invokeWithArguments", "(Ljava/util/List;)Ljava/lang/Object;"),
        (mh, "bindTo", "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;"),
    ];
    assert_eq!(registry.reentry_only_bodies(), &want[..]);
    for (c, m, d) in want {
        assert_eq!(
            registry.find_with_kind(c, m, d).map(|(_, kind)| kind),
            Some(cratonvm_native_api::NativeKind::Intrinsic),
            "{c}.{m}{d} stays an Intrinsic"
        );
    }
}
}

/// JIT round 21 wave 10, lane indy: `MHN_resolve`'s refused method access
/// spells HotSpot 25's `LinkResolver::check_method_accessability` text.
#[cfg(test)]
mod r21_w10_mhn_access_text_tests {
use super::*;

#[test]
fn external_method_names_follow_print_external_name() {
    assert_eq!(
        mhn_external_method_name("p.D", "m", "()V").as_deref(),
        Some("void p.D.m()")
    );
    assert_eq!(
        mhn_external_method_name("p.D", "<init>", "(ILjava/lang/String;)V").as_deref(),
        Some("void p.D.<init>(int, java.lang.String)")
    );
    assert_eq!(
        mhn_external_method_name("p.D", "f", "([[J[Lq/E;Z)[Ljava/lang/Object;").as_deref(),
        Some("java.lang.Object[] p.D.f(long[][], q.E[], boolean)")
    );
    assert_eq!(
        mhn_external_method_name("p.D", "m", "(V)V").as_deref(),
        Some("void p.D.m(void)")
    );
    assert_eq!(mhn_external_method_name("p.D", "m", "()"), None);
    assert_eq!(mhn_external_method_name("p.D", "m", "(Lp/X)V"), None);
    assert_eq!(mhn_external_method_name("p.D", "m", "()[V"), None);
}

#[test]
fn private_method_text_with_joint_unnamed_module_clause() {
    let clause = mhn_member_module_clause(
        ("q.C", "unnamed module", "'app'"),
        ("p.D", "unnamed module", "'app'"),
        true,
    );
    assert_eq!(
        mhn_method_access_denied_text("q.C", MHN_ACC_PRIVATE, "void p.D.m()", Some(&clause)),
        "class q.C tried to access private method 'void p.D.m()' \
         (q.C and p.D are in unnamed module of loader 'app')"
    );
}

#[test]
fn package_and_protected_texts_and_two_module_clause() {
    let clause = mhn_member_module_clause(
        ("q.C", "unnamed module", "'app'"),
        ("java.lang.Thing", "module java.base", "'bootstrap'"),
        false,
    );
    assert_eq!(
        mhn_method_access_denied_text("q.C", 0, "int java.lang.Thing.m()", Some(&clause)),
        "class q.C tried to access method 'int java.lang.Thing.m()' \
         (q.C is in unnamed module of loader 'app'; java.lang.Thing is in module \
         java.base of loader 'bootstrap')"
    );
    // abstract, protected, private: HotSpot's order.
    assert_eq!(
        mhn_method_access_denied_text("q.C", 0x0004 | MHN_ACC_ABSTRACT, "void p.D.m()", None),
        "class q.C tried to access abstract protected method 'void p.D.m()'"
    );
    assert_eq!(
        mhn_method_access_denied_text(
            "q.C",
            MHN_ACC_PRIVATE | MHN_ACC_STATIC,
            "void p.D.<init>()",
            None
        ),
        "class q.C tried to access private method 'void p.D.<init>()'"
    );
}
}

/// JIT round 22 wave 3, lane runtime (`CRATONVM_MH_LANES_KEEP_CUSTOMIZATION`,
/// page `r22-ctor-c0-hidden-frame-names-the-holder-not-the-customized-form-20261020`):
/// the before-linker lanes' `checkCustomized` count -- raised below the
/// threshold, the due call declined to the linker, a customized form left
/// alone, the take-back of a declined lane, and `=0` the old (no count) answer.
#[cfg(test)]
mod r22w3_runtime_lanes_keep_customization_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeSystemAccess};
use cratonvm_types::flags::with_thread_overrides;

fn handle(ctx: &mut MockNativeContext) -> (ObjectRef, ObjectRef) {
    let field = |name: &str, desc: &str, slot: usize, cid: ClassId| {
        cratonvm_native_api::FieldMetadata {
            name: name.to_string(),
            descriptor: desc.to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: cid,
            is_static: false,
        }
    };
    let (mh_cid, form_cid) = (ClassId::new(0x7201), ClassId::new(0x7202));
    ctx.set_declared_fields(
        mh_cid,
        vec![
            field("form", "Ljava/lang/invoke/LambdaForm;", 0, mh_cid),
            field("customizationCount", "B", 1, mh_cid),
        ],
    );
    ctx.set_declared_fields(
        form_cid,
        vec![field("customized", "Ljava/lang/invoke/MethodHandle;", 0, form_cid)],
    );
    let mh = ctx.alloc_object(mh_cid, 2);
    let form = ctx.alloc_object(form_cid, 1);
    ctx.set_field_by_name(mh, "form", Value::Object(Some(form)));
    ctx.set_field_by_name(form, "customized", Value::Object(None));
    ctx.set_field_by_name(mh, "customizationCount", Value::Int(0));
    (mh, form)
}

fn count(ctx: &MockNativeContext, mh: ObjectRef) -> Value {
    ctx.get_field_by_name(mh, "customizationCount")
}


/// Every before-linker lane asks the count (a lane added later without it
/// is the defect this page records).
#[test]
fn r22w3_every_before_linker_lane_asks_the_count() {
    let src: &'static str = LANG_INVOKE_SRC;
    for f in [
        "fn ctor_lane_counted_instantiate(",
        "fn mh_real_direct_before_linker(",
        "fn mh_real_field_before_linker(",
        "fn real_bound_field_serve(",
        "pub fn jit_real_field_lane_warm(",
        // Wave 4: the converting `invoke` lane counts the `asTypeCache` adapter.
        "fn mh_real_field_converting_serve(",
    ] {
        let at = src.find(f).expect(f);
        let end = src[at..].find("\n}\n").expect("fn end");
        assert!(
            src[at..at + end].contains("lane_check_customized("),
            "{f} serves without checkCustomized's count"
        );
    }
    let at = src.find("fn mh_real_constructor_fast(").expect("ctor lane");
    let end = src[at..].find("\n}\n").expect("fn end");
    let body = &src[at..at + end];
    assert!(!body.contains("ctor_lane_instantiate(ctx"), "the ctor lane counts first");
    assert!(body.contains("hidden_frames_never_observed()"));
}
}

/// Interpreter round i7 wave 135, lane L4 (page i110-L4, "Wave 135"): the two
/// invoker-check `Intrinsic`s read by the per-VM slots, and answer what the
/// by-name reads answered (count raised below the threshold, a customized
/// form left alone, the identical type accepted).
#[cfg(test)]
mod i135_l4_invoker_checks_slot_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeSystemAccess};

#[test]
fn i135_l4_check_customized_and_exact_type_by_slot() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x7135_0004);
    let mh_cid = ctx.ensure_class_initialized("java/lang/invoke/MethodHandle").expect("mh");
    let lf_cid = ctx.ensure_class_initialized("java/lang/invoke/LambdaForm").expect("lf");
    let field = |name: &str, slot: usize, cid: ClassId| cratonvm_native_api::FieldMetadata {
        name: name.to_string(),
        descriptor: "Ljava/lang/Object;".to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: cid,
        is_static: false,
    };
    ctx.set_declared_fields(
        mh_cid,
        vec![field("type", 0, mh_cid), field("form", 1, mh_cid), field("customizationCount", 4, mh_cid)],
    );
    ctx.set_declared_fields(lf_cid, vec![field("customized", 2, lf_cid)]);
    let mh = ctx.alloc_object(mh_cid, 6);
    let form = ctx.alloc_object(lf_cid, 3);
    let mt = ctx.alloc_object(lf_cid, 3);
    ctx.set_field(mh, 0, Value::Object(Some(mt)));
    ctx.set_field(mh, 1, Value::Object(Some(form)));
    ctx.set_field(form, 2, Value::Object(None));
    ctx.set_field(mh, 4, Value::Int(5));
    assert_eq!(invokers_mh_type_slot(&ctx, mh), Some(0));
    let r = invokers_check_customized_intrinsic(&mut ctx, &[Value::Object(Some(mh))]);
    assert!(matches!(r, Ok(None)));
    assert_eq!(ctx.get_field(mh, 4), Value::Int(6), "counted by slot");
    ctx.set_field(form, 2, Value::Object(Some(mh)));
    let r = invokers_check_customized_intrinsic(&mut ctx, &[Value::Object(Some(mh))]);
    assert!(matches!(r, Ok(None)));
    assert_eq!(ctx.get_field(mh, 4), Value::Int(6), "a customized form counts nothing");
    let r = invokers_check_exact_type_intrinsic(
        &mut ctx,
        &[Value::Object(Some(mh)), Value::Object(Some(mt))],
    );
    assert!(matches!(r, Ok(None)), "the identical type is accepted by slot");
}
}

/// Interpreter round i7 wave 137, lane L4 (`CRATONVM_MH_LF_LEAF_INTRINSICS`):
/// the two leaf helpers' plain answers.
#[cfg(test)]
mod i137_l4_lf_leaf_intrinsic_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeSystemAccess};

#[test]
fn i137_l4_internal_member_name_by_slot_and_empty_spread() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x7137_0004);
    let dmh_cid = ctx.ensure_class_initialized(LF_LEAF_DMH_CLASS).expect("dmh");
    ctx.set_declared_fields(
        dmh_cid,
        vec![cratonvm_native_api::FieldMetadata {
            name: "member".to_string(),
            descriptor: "Ljava/lang/invoke/MemberName;".to_string(),
            access_flags: 0x0010,
            slot_index: 2,
            declaring_class_id: dmh_cid,
            is_static: false,
        }],
    );
    let dmh = ctx.alloc_object(dmh_cid, 4);
    let member = ctx.alloc_object(dmh_cid, 4);
    ctx.set_field(dmh, 2, Value::Object(Some(member)));
    assert_eq!(dmh_member_slot(&ctx, dmh), Some(2));
    let r = dmh_internal_member_name_intrinsic(&mut ctx, &[Value::Object(Some(dmh))]);
    assert!(
        matches!(r, Ok(Some(Value::Object(Some(m)))) if m.as_ptr() == member.as_ptr()),
        "the member slot answers"
    );
    // A second call takes the admitted-class word.
    let r = dmh_internal_member_name_intrinsic(&mut ctx, &[Value::Object(Some(dmh))]);
    assert!(matches!(r, Ok(Some(Value::Object(Some(_))))));
    let r = mhi_check_spread_argument_intrinsic(&mut ctx, &[Value::Object(None), Value::Int(0)]);
    assert!(matches!(r, Ok(None)), "a null array of length 0 is accepted");
}

/// `CRATONVM_MHN_RESOLVE_SPECIAL_LINK_CHECKS`: `LinkResolver`'s three
/// static-ness wordings (`L4W137FindSpecialAccess`).
#[test]
fn i137_l4_static_mismatch_wordings() {
    let mut ctx = MockNativeContext::new();
    let d = ctx.ensure_class_initialized("p/D").expect("p/D");
    assert_eq!(
        mhn_static_mismatch_message(&ctx, 6, false, d, "m", "()Ljava/lang/String;"),
        "Expected static method 'java.lang.String p.D.m()'"
    );
    assert_eq!(
        mhn_static_mismatch_message(&ctx, 7, false, d, "m", "(I)V"),
        "Expecting non-static method 'void p.D.m(int)'"
    );
    assert_eq!(
        mhn_static_mismatch_message(&ctx, 9, true, d, "m", "()I"),
        "Expected instance not static method 'int p.D.m()'"
    );
    assert_eq!(mhn_special_indirect_superinterface(&ctx, d, None, d, "m", "()V"), None);
    assert_eq!(mhn_special_indirect_superinterface(&ctx, d, Some(d), d, "m", "()V"), None);
}
}

/// Interpreter round i7 wave 136, lane L3 (cross-review of wave 135 L4):
/// `CUSTOMIZE_THRESHOLD` is the JDK's own static once `MethodHandleStatics`
/// is initialized (a property written at run time moves nothing, as on
/// HotSpot), and the property parsed as `Integer.parseInt` before that.
#[cfg(test)]
mod i136_l3_customize_threshold_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeSystemAccess};

const KEY: &str = "java.lang.invoke.MethodHandle.CUSTOMIZE_THRESHOLD";

#[test]
fn i136_l3_the_jdk_static_wins_over_a_later_property_write() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x7136_0301);
    let statics = ctx.ensure_class_initialized("java/lang/invoke/MethodHandleStatics").expect("mhs");
    ctx.set_declared_fields(
        statics,
        vec![cratonvm_native_api::FieldMetadata {
            name: "CUSTOMIZE_THRESHOLD".to_string(),
            descriptor: "I".to_string(),
            access_flags: 0x0018,
            slot_index: 3,
            declaring_class_id: statics,
            is_static: true,
        }],
    );
    ctx.set_static_field(statics, 3, Value::Int(MH_CUSTOMIZE_THRESHOLD));
    ctx.set_system_property(KEY, "-1");
    assert_eq!(customize_threshold(&ctx), MH_CUSTOMIZE_THRESHOLD);
    assert!(customize_threshold_is_default(&ctx));
}

#[test]
fn i136_l3_before_the_clinit_the_property_is_parsed_uncached() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x7136_0302);
    assert_eq!(customize_threshold(&ctx), MH_CUSTOMIZE_THRESHOLD, "no property");
    ctx.set_system_property(KEY, "+127");
    assert!(customize_threshold_is_default(&ctx));
    ctx.set_system_property(KEY, "0");
    assert_eq!(customize_threshold(&ctx), 0, "not cached before the clinit");
    ctx.set_system_property(KEY, "-1");
    assert_eq!(customize_threshold(&ctx), -1);
    ctx.set_system_property(KEY, "-2");
    assert_eq!(customize_threshold(&ctx), MH_CUSTOMIZE_THRESHOLD_UNUSABLE);
    ctx.set_system_property(KEY, "x");
    assert!(!customize_threshold_is_default(&ctx));
}
}

/// Interpreter round i9 (page i147-orch): `CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE`.
/// Every switch is set through a thread override; no default is pinned. The
/// end-to-end rows (cast rejections, `<init>` throwables, customization, a
/// stack walk from `<init>`, hand-built handles) are run against HotSpot.
/// Restored by t1 lane H (2026-10-25) with the lane the lang_invoke
/// decomposition had dropped; `the_lane_and_its_call_are_present` pins that.


/// JIT round 21, lane vhmerge
/// (`docs/feature-designs/native-dispatch-architecture.md`;
/// probes `R20FuzzGVhArrayAll1` refuse code 10, `R20FuzzIVhArr1` counts):
/// the static value check that runs BEFORE an array handle's coordinate cast
/// judges the values against the HANDLE's component type, so a wrong array
/// passed as `Object` is the cast's `ClassCastException`, while a value the
/// handle's own type refuses is still the earlier `WrongMethodTypeException`.
#[cfg(test)]
mod jr21_vhmerge_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::poly_call_site::{arm_vh_param_site, vh_param_site_classes};
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

fn obj(ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
    match ctx.new_object(class).unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    }
}

fn handle(ctx: &mut MockNativeContext, class_name: &str, desc: &str) -> ObjectRef {
    let vh = obj(ctx, "java/lang/invoke/VarHandle");
    vh_meta_put(
        ctx,
        vh,
        VarHandleMeta {
            kind: VH_KIND_ARRAY,
            class_name: class_name.to_string(),
            field_name: String::new(),
            field_desc: desc.to_string(),
            field_index: -1,
            class_id: 0,
            read_only: false,
            exact: None,
            ref_type_class_id: 0,
            holder_class_id: 0,
        },
    );
    vh
}

fn is_cce(err: &MethodCallFailed) -> bool {
    matches!(
        err,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { .. }
        ))
    )
}

fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
    match err {
        MethodCallFailed::ExceptionThrown(o) => {
            ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                == Some("java/lang/invoke/WrongMethodTypeException")
        }
        _ => false,
    }
}

/// The judged variable type is the handle's, not the passed array's.
#[test]
fn the_static_value_type_is_the_handles_component() {
    if !vh_array_coordinate_cast_enabled() {
        return; // an explicit `0` in the test environment wins
    }
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x21AB_0001);
    let zh = handle(&mut ctx, "[Z", "Z");
    let ints = ctx.new_array(ArrayElementType::Int, 4);
    let args = [
        Value::Object(Some(zh)),
        Value::Object(Some(ints)),
        Value::Int(0),
        Value::Int(1),
    ];
    let meta = vh_meta_get(&mut ctx, zh);
    assert_eq!(
        vh_static_value_var_desc(&mut ctx, zh, meta.as_deref(), &args),
        Some(b'Z')
    );
    // The run-time reading (right only after the cast) says `int`.
    assert_eq!(
        vh_primitive_var_desc(&mut ctx, meta.as_deref(), &args),
        Some(b'I')
    );
    // A reference-array handle has no primitive variable.
    let oh = handle(&mut ctx, "[Ljava/lang/Object;", "Ljava/lang/Object;");
    let meta = vh_meta_get(&mut ctx, oh);
    let args = [
        Value::Object(Some(oh)),
        Value::Object(Some(ints)),
        Value::Int(0),
        Value::Int(1),
    ];
    assert_eq!(
        vh_static_value_var_desc(&mut ctx, oh, meta.as_deref(), &args),
        None
    );
}

/// `R20FuzzGVhArrayAll1` case 10 and `R20FuzzIVhArr1`'s `viaObject`:
/// a wrong primitive array through an `Object` coordinate is CCE for a
/// value mode, as on HotSpot 25; nothing is stored.
#[test]
fn a_wrong_primitive_array_in_a_value_mode_is_cce_not_wmte() {
    if !vh_array_coordinate_cast_enabled() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x21AB_0002);
    let zv = Value::Object(Some(handle(&mut ctx, "[Z", "Z")));
    let lv = Value::Object(Some(handle(&mut ctx, "[J", "J")));
    let iv = Value::Object(Some(handle(&mut ctx, "[I", "I")));
    let ints = ctx.new_array(ArrayElementType::Int, 4);
    let bytes = ctx.new_array(ArrayElementType::Byte, 4);
    let a = Value::Object(Some(ints));
    let b = Value::Object(Some(bytes));
    // VZ.getAndBitwiseOr((Object) ia, 0, true)
    arm_vh_param_site(vh_param_site_classes("(Ljava/lang/Object;IZ)Z"));
    let err = varhandle_get_and_bitwise_or(&mut ctx, &[zv, a, Value::Int(0), Value::Int(1)])
        .unwrap_err();
    assert!(is_cce(&err), "boolean[] handle, int[] array: {err:?}");
    // LA.set((Object) ints, 1, 9L)
    arm_vh_param_site(vh_param_site_classes("(Ljava/lang/Object;IJ)V"));
    let err = varhandle_set(&mut ctx, &[lv, a, Value::Int(1), Value::Long(9)]).unwrap_err();
    assert!(is_cce(&err), "long[] handle, int[] array: {err:?}");
    assert_eq!(ctx.get_array_element(ints, 1), Value::Int(0));
    // IA.compareAndSet((Object) bytes, 1, 0, 1)
    arm_vh_param_site(vh_param_site_classes("(Ljava/lang/Object;III)Z"));
    let err = varhandle_compare_and_set(
        &mut ctx,
        &[iv, b, Value::Int(1), Value::Int(0), Value::Int(1)],
    )
    .unwrap_err();
    assert!(is_cce(&err), "int[] handle, byte[] array: {err:?}");
    // Positive control (dev's order fix stays): a value the HANDLE's own
    // type refuses is WMTE, on the right array too, and stores nothing.
    arm_vh_param_site(vh_param_site_classes("(Ljava/lang/Object;IJ)V"));
    let err = varhandle_set(&mut ctx, &[iv, a, Value::Int(1), Value::Long(9)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "int[] handle, long value: {err:?}");
    assert_eq!(ctx.get_array_element(ints, 1), Value::Int(0));
}
}

/// j27 wave 9, lane W9EXC: an instance-field VarHandle access with a null
/// holder lists HotSpot's `VarHandle<T>s$FieldInstance*.<mode>` ->
/// `Objects.requireNonNull` frames (`J27NpeHiddenRows` `vh-get-null-holder`).
mod j27_vh_null_holder_chain_tests {
use super::*;
use cratonvm_native_api::ServedRaiseHopSite;

#[test]
fn instance_family_takes_the_holder_as_object() {
    assert_eq!(
        vh_field_mode_method("get", b'I', true),
        Some((
            "java/lang/invoke/VarHandleInts$FieldInstanceReadOnly",
            "(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;)I"
        ))
    );
    assert_eq!(
        vh_field_mode_method("set", b'L', true),
        Some((
            "java/lang/invoke/VarHandleReferences$FieldInstanceReadWrite",
            "(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;Ljava/lang/Object;)V"
        ))
    );
    assert_eq!(
        vh_field_mode_method("getAndAdd", b'J', true),
        Some((
            "java/lang/invoke/VarHandleLongs$FieldInstanceReadWrite",
            "(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;J)J"
        ))
    );
    // The static family is unchanged by the generalisation.
    assert_eq!(vh_field_mode_method("get", b'I', false), vh_static_mode_method("get", b'I'));
}

#[test]
fn null_holder_chain_tops_out_at_require_non_null() {
    let chain = vh_null_holder_chain("set", b'I').unwrap();
    assert_eq!(chain.throwable_class, "java/lang/NullPointerException");
    let hops: Vec<(&str, &str)> = chain.hops.iter().map(|h| (h.class, h.name)).collect();
    assert_eq!(
        hops,
        [
            ("java/lang/invoke/VarHandle", "set"),
            ("java/lang/invoke/VarHandleInts$FieldInstanceReadWrite", "set"),
            ("java/util/Objects", "requireNonNull"),
        ]
    );
    assert_eq!(chain.hops[0].site, ServedRaiseHopSite::Hidden);
    assert_eq!(chain.hops[2].site, ServedRaiseHopSite::Raise);
    assert!(vh_null_holder_chain("toMethodHandle", b'I').is_none());
    assert!(vh_null_holder_chain("get", 0).is_none());
}
}
