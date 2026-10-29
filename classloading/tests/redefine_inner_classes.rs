// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 21 wave 17, lane innerclasses (`CRATONVM_REDEFINE_SWAPS_INNER_CLASSES`;
//! page `docs/feature-designs/class-resolution-and-caching.md` item 5).
//!
//! HotSpot's `VM_RedefineClasses::redefine_single_class` swaps the new
//! version's `InnerClasses` (and the `EnclosingMethod` indices it carries) into
//! the redefined class. A changed table is never refused; a malformed one is the
//! parser's `ClassFormatError` (`JVMTI_ERROR_INVALID_CLASS_FORMAT`) for every
//! loader but the boot loader.

use cratonvm_classloading::{
    ClassLoaderId, ClassManager, DefineClassOptions, RedefineOptions, RedefinitionRefusal,
};

const NAME: &str = "w17/Inn$1";

/// The shape of the class's `InnerClasses` self row and `EnclosingMethod`.
enum Nesting {
    /// `(this, 0, 0)` plus `EnclosingMethod(w17/Inn, 0)`: anonymous.
    Anonymous,
    /// `(this, w17/Inn, "One")`, no `EnclosingMethod`: a member.
    Member,
    /// `(this, this, "One")`: "Class is both outer and inner class".
    SelfOuter,
    /// [`Nesting::Member`] with these raw `inner_class_access_flags`.
    MemberWithFlags(u16),
}

/// A Java 8 class `w17/Inn$1 extends java/lang/Object`, no fields or methods,
/// with the same constant pool in every shape (so only the attributes differ).
fn class_bytes(nesting: Nesting) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&(s.len() as u16).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    fn class(out: &mut Vec<u8>, name_index: u16) {
        out.push(7);
        out.extend_from_slice(&name_index.to_be_bytes());
    }
    fn u16s(out: &mut Vec<u8>, values: &[u16]) {
        for v in values {
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    u16s(&mut b, &[10]); // constant_pool_count
    utf8(&mut b, NAME); // #1
    class(&mut b, 1); // #2
    utf8(&mut b, "java/lang/Object"); // #3
    class(&mut b, 3); // #4
    utf8(&mut b, "InnerClasses"); // #5
    utf8(&mut b, "w17/Inn"); // #6
    class(&mut b, 6); // #7
    utf8(&mut b, "One"); // #8
    utf8(&mut b, "EnclosingMethod"); // #9
    // public super; this; super; no interfaces, fields or methods.
    u16s(&mut b, &[0x0021, 2, 4, 0, 0, 0]);
    let inner_row: [u16; 4] = match nesting {
        Nesting::Anonymous => [2, 0, 0, 0],
        Nesting::Member => [2, 7, 8, 0],
        Nesting::SelfOuter => [2, 2, 8, 0],
        Nesting::MemberWithFlags(flags) => [2, 7, 8, flags],
    };
    let enclosing = matches!(nesting, Nesting::Anonymous);
    u16s(&mut b, &[if enclosing { 2 } else { 1 }]); // attributes_count
    u16s(&mut b, &[5]);
    b.extend_from_slice(&10u32.to_be_bytes()); // 2 + one 8-byte row
    u16s(&mut b, &[1]);
    u16s(&mut b, &inner_row);
    if enclosing {
        u16s(&mut b, &[9]);
        b.extend_from_slice(&4u32.to_be_bytes());
        u16s(&mut b, &[7, 0]);
    }
    b
}

fn define(cm: &mut ClassManager, nesting: Nesting, loader: ClassLoaderId) -> cratonvm_classloading::ClassId {
    cm.define_class_with_options(NAME, &class_bytes(nesting), loader, DefineClassOptions::default())
        .expect("define the fixture")
}

fn switch_on() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_REDEFINE_SWAPS_INNER_CLASSES")
}

#[test]
fn a_redefinition_installs_the_new_inner_classes_and_enclosing_method() {
    if !switch_on() {
        return;
    }
    let mut cm = ClassManager::new(&[], &[], &[]);
    let cid = define(&mut cm, Nesting::Anonymous, ClassLoaderId::Application);
    {
        let class = cm.get_class(cid).expect("defined");
        assert_eq!(class.inner_classes.len(), 1);
        assert!(class.inner_classes[0].outer_class.is_empty());
        assert!(class.enclosing_method.is_some(), "precondition: EnclosingMethod");
    }
    cm.redefine_class(cid, class_bytes(Nesting::Member), RedefineOptions::default())
        .expect("HotSpot never refuses a changed InnerClasses");
    let class = cm.get_class(cid).expect("redefined");
    assert_eq!(class.inner_classes.len(), 1);
    assert_eq!(class.inner_classes[0].inner_class, NAME);
    assert_eq!(class.inner_classes[0].outer_class, "w17/Inn");
    assert_eq!(class.inner_classes[0].inner_name, "One");
    assert!(
        class.enclosing_method.is_none(),
        "the EnclosingMethod indices live in the swapped array"
    );
}

#[test]
fn a_malformed_inner_classes_refuses_the_redefinition_and_its_check() {
    if !switch_on() {
        return;
    }
    let mut cm = ClassManager::new(&[], &[], &[]);
    let cid = define(&mut cm, Nesting::Member, ClassLoaderId::Application);
    let bad = class_bytes(Nesting::SelfOuter);
    let checked = cm
        .check_redefinition(cid, &bad, &RedefineOptions::default())
        .expect_err("the batch check refuses before any class changes");
    assert_eq!(checked.kind, RedefinitionRefusal::InvalidClassFormat);
    let refused = cm
        .redefine_class_typed(cid, bad, RedefineOptions::default())
        .expect_err("ClassFormatError: Class is both outer and inner class");
    assert_eq!(refused.kind, RedefinitionRefusal::InvalidClassFormat);
    let class = cm.get_class(cid).expect("still defined");
    assert_eq!(class.inner_classes[0].outer_class, "w17/Inn", "nothing installed");
    assert_eq!(
        RedefinitionRefusal::InvalidClassFormat.instrument_throwable().0,
        "java/lang/ClassFormatError"
    );
}

/// JIT round 22 wave 1, lane review (item 7 of
/// `docs/feature-designs/class-resolution-and-caching.md`): the
/// redefinition installs the new entries' flags as the define path stores
/// them (`hotspot_stored_inner_classes`): an unrecognised bit (`0x0080`) is
/// masked off a `public static final` member, as HotSpot's scratch class's
/// parser masks it.
#[test]
fn a_redefinition_masks_the_new_inner_class_flags_as_the_define_does() {
    if !switch_on() {
        return;
    }
    let mut cm = ClassManager::new(&[], &[], &[]);
    let defined = define(&mut cm, Nesting::MemberWithFlags(0x0099), ClassLoaderId::Application);
    assert_eq!(
        cm.get_class(defined).expect("defined").inner_classes[0].access_flags,
        0x0019,
        "the define path masks the entry's flags"
    );
    let mut cm = ClassManager::new(&[], &[], &[]);
    let cid = define(&mut cm, Nesting::Anonymous, ClassLoaderId::Application);
    cm.redefine_class(cid, class_bytes(Nesting::MemberWithFlags(0x0099)), RedefineOptions::default())
        .expect("HotSpot never refuses a changed InnerClasses");
    let class = cm.get_class(cid).expect("redefined");
    assert_eq!(class.inner_classes[0].outer_class, "w17/Inn");
    assert_eq!(class.inner_classes[0].access_flags, 0x0019, "masked like the define");
}
