use super::{count_reported_unloads, reported_unload};
use crate::classloading::{ClassId, ClassLoaderId};
use cratonvm_classloading::UnloadedClass;

fn unloaded(id: u32, name: &str) -> UnloadedClass {
    UnloadedClass {
        id: ClassId::new(id),
        name: std::sync::Arc::from(name),
        loader_id: ClassLoaderId::UserDefined(0x12_0007),
    }
}

#[test]
fn gci_i12_ul_array_classes_are_not_reported_unloads() {
    let batch = [
        unloaded(1, "p/X"),
        unloaded(2, "[Lp/X;"),
        unloaded(3, "[[Lp/X;"),
        unloaded(4, "p/X$$Lambda/0x0000000800001000"),
    ];
    assert!(reported_unload(&batch[0]));
    assert!(!reported_unload(&batch[1]));
    assert!(!reported_unload(&batch[2]));
    assert!(reported_unload(&batch[3]), "a hidden class counts, as on HotSpot");
    assert_eq!(count_reported_unloads(&batch), 2);
    assert_eq!(count_reported_unloads(&[]), 0);
}

/// gci i12/ul F3: the new loader's body keeps its live `p/X`; the old
/// loader's and the parent's bodies do not; off the switch nothing is kept.
#[test]
fn gci_i12_ul_live_namesakes_answer_per_body_loader() {
    let new_loader = ClassLoaderId::UserDefined(0x12_0008);
    let mut live = super::LiveNamesakes::default();
    assert!(!live.is_armed());
    assert!(!live.has_live("p/X"));
    assert!(!live.body_sees_live(ClassId::new(77), "p/X"));
    live.armed = true;
    live.definers
        .entry("p/X".to_string())
        .or_default()
        .insert(new_loader);
    live.loader_of.insert(77, new_loader);
    live.loader_of.insert(5, ClassLoaderId::Application);
    assert!(live.has_live("p/X"));
    assert!(!live.has_live("p/Y"));
    assert!(live.body_sees_live(ClassId::new(77), "p/X"), "the new loader's body");
    assert!(!live.body_sees_live(ClassId::new(5), "p/X"), "a parent's body");
    assert!(!live.body_sees_live(ClassId::new(41), "p/X"), "an unknown body");
    assert!(!live.body_sees_live(ClassId::new(77), "p/Y"));
}
