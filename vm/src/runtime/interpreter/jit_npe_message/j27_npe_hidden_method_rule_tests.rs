use super::{trap_site_is_hidden_in, TrapSite};
use crate::classloading::{ClassId, ClassStore};
use crate::runtime::stackwalker::npe_top_frame_is_hidden_with;
use crate::runtime::stackwalker::test_support::{named_method, store_with};
use cratonvm_reader::attribute::{Annotation, Attribute, LazyAttribute};
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry, CpUtf8};
use std::sync::Arc;

const HIDDEN_DESC: &str = "(Ljava/lang/Object;)I";

/// A store holding `name` with `plain()V` and an `@Hidden` `hid(Object)I`,
/// moved to a non-zero id (a recorded id of `0` means "none").
fn carrier(name: &str, loader: crate::classloading::ClassLoaderId) -> (ClassStore, ClassId) {
    let mut hid = named_method("hid", HIDDEN_DESC, Vec::new());
    hid.attributes.push(LazyAttribute::new_decoded(
        Attribute::RuntimeVisibleAnnotations(vec![Annotation {
            type_index: 1,
            element_value_pairs: Vec::new(),
        }]),
    ));
    let (mut store, id) = store_with(vec![named_method("plain", "()V", Vec::new()), hid]);
    let mut class = store.remove(id).expect("the fixture's class");
    class.id = store.next_id();
    class.name = Arc::from(name);
    class.loader_id = loader;
    class.constant_pool = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8(CpUtf8::from_arc(Arc::from(
            "Ljdk/internal/vm/annotation/Hidden;",
        ))),
    ]);
    let id = store.add(class);
    assert_ne!(id.as_u32(), 0);
    (store, id)
}

fn site(class_name: &str, method: &str, desc: &str, class_id: u32) -> TrapSite {
    TrapSite {
        class_name: class_name.into(),
        method_name: method.into(),
        method_descriptor: desc.into(),
        bci: 1,
        exact: true,
        class_id,
    }
}

/// The defect: an `@Hidden` method of a boot class (a `*$Holder` lambda
/// form, `VarHandleGuards.guard_*`) is a hidden top frame, so no message;
/// its class's other methods, and the same method with the rule's kill
/// switch off (the class-bit rule), still get one.
#[test]
fn an_at_hidden_method_of_a_boot_class_is_a_hidden_top_frame() {
    use crate::classloading::ClassLoaderId;
    let name = "java/lang/invoke/J27NpeHiddenCarrier$Holder";
    let (store, id) = carrier(name, ClassLoaderId::Bootstrap);
    let on = || true;
    let off = || false;
    assert!(npe_top_frame_is_hidden_with(&store, id, "hid", HIDDEN_DESC, on));
    assert!(!npe_top_frame_is_hidden_with(&store, id, "plain", "()V", on));
    assert!(!npe_top_frame_is_hidden_with(&store, id, "hid", HIDDEN_DESC, off));
    // The compiled trap site asks the same rule, by recorded id and by name.
    let raw = id.as_u32();
    assert!(trap_site_is_hidden_in(&store, &site(name, "hid", HIDDEN_DESC, raw)));
    assert!(trap_site_is_hidden_in(&store, &site(name, "hid", HIDDEN_DESC, 0)));
    assert!(!trap_site_is_hidden_in(&store, &site(name, "plain", "()V", raw)));
}

/// HotSpot honours `@Hidden` only in privileged code: an application
/// loader's ordinary class is not hidden by the annotation.
#[test]
fn an_at_hidden_method_of_a_user_class_is_not_hidden() {
    use crate::classloading::ClassLoaderId;
    let name = "probe/J27NpeHiddenUser";
    let (store, id) = carrier(name, ClassLoaderId::Application);
    assert!(!npe_top_frame_is_hidden_with(&store, id, "hid", HIDDEN_DESC, || true));
    assert!(!trap_site_is_hidden_in(&store, &site(name, "hid", HIDDEN_DESC, id.as_u32())));
}

/// Every method of a hidden class is hidden under both rules, and a
/// `/0x..` class the store no longer holds is still taken as hidden (the
/// old answer); an unknown ordinary class is not.
#[test]
fn a_hidden_class_is_hidden_under_both_rules() {
    use crate::classloading::ClassLoaderId;
    let name = "probe/J27NpeHiddenLambda/0x1234";
    let (mut store, id) = carrier(name, ClassLoaderId::Application);
    store.get_mut(id).expect("class").hidden = true;
    for rule in [(|| true) as fn() -> bool, || false] {
        assert!(npe_top_frame_is_hidden_with(&store, id, "plain", "()V", rule));
    }
    assert!(trap_site_is_hidden_in(&store, &site(name, "plain", "()V", id.as_u32())));
    let gone = "probe/J27NpeHiddenGone/0x99";
    assert!(trap_site_is_hidden_in(&store, &site(gone, "m", "()V", 0)));
    assert!(!trap_site_is_hidden_in(
        &store,
        &site("java/lang/J27NpeHiddenAbsent", "m", "()V", 0)
    ));
}
