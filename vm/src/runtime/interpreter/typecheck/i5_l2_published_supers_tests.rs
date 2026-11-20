use super::{aastore_element_assignable, class_is_subtype, published_subtype_verdict};
use crate::classloading::ClassId;
use crate::runtime::interpreter::site_cache::{CastSite, CastSiteCache};
use cratonvm_types::ArrayElementType;
use std::sync::Arc;

fn class(shared: &crate::vm::SharedVm, name: &str) -> ClassId {
    let mut cm = shared.classes.class_manager.write();
    cm.try_ensure_synthetic_class(name, 0)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk")
}

/// Stage 3 of the subtype display: the realm's view IS the class store's,
/// a locked question publishes the receiver's closure, after which the
/// same receiver is answered — positively and negatively — with no lock,
/// and an edge change withdraws it until the next locked question.
#[test]
fn a_subtype_answer_is_published_by_the_first_question_and_withdrawn_by_an_edge_change() {
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let base = class(&shared, "cratonvm/test/I5L2PubBase");
    let sub = class(&shared, "cratonvm/test/I5L2PubSub");
    let other = class(&shared, "cratonvm/test/I5L2PubOther");
    let store_view = shared
        .classes
        .class_manager
        .read()
        .class_store
        .published_supers();
    assert!(Arc::ptr_eq(&store_view, &shared.classes.published_supers));
    shared
        .classes
        .class_manager
        .write()
        .set_superclass(sub, Some(base));
    assert!(class_is_subtype(&shared, sub, base));
    let Some(published) = published_subtype_verdict(&shared, sub, base) else {
        // `CRATONVM_LOADER_NO_SUBTYPE_DISPLAY=1`: nothing is published and
        // every question is asked under the lock, as before stage 3.
        return;
    };
    assert!(published);
    assert!(!class_is_subtype(&shared, sub, other));
    assert_eq!(published_subtype_verdict(&shared, sub, other), Some(false));

    // Re-parent: the closure is withdrawn; the next question republishes.
    shared
        .classes
        .class_manager
        .write()
        .set_superclass(sub, Some(other));
    assert_eq!(published_subtype_verdict(&shared, sub, base), None);
    assert!(!class_is_subtype(&shared, sub, base));
    assert!(class_is_subtype(&shared, sub, other));
    assert_eq!(published_subtype_verdict(&shared, sub, other), Some(true));

    // `aastore` of a `Sub` into an `Other[]`: the lock-free positive arm.
    let arr = shared
        .mem
        .heap
        .alloc_array(other, ArrayElementType::Reference, 1);
    let val = shared.mem.heap.alloc_object(sub, 0);
    assert!(aastore_element_assignable(&shared, arr, val));
}

/// A cast-site memo (positive or negative receiver) is a subtype verdict,
/// so an edge change under unchanged ids must retire it. The four
/// `ClassStore` edge mutators move `class_definition_epoch`, which every
/// memo carries (`CastSite::memo_epoch`, i7-L2; the entry itself is
/// tagged with the name generation and may keep its resolution), and a
/// reader sees the memos only through `CastSite::observed`.
#[test]
fn an_edge_change_retires_every_cast_site_memo() {
    use crate::runtime::interpreter::site_cache::cast_memo_epoch_now;
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let target = class(&shared, "cratonvm/test/I5L2MemoTarget");
    let receiver = class(&shared, "cratonvm/test/I5L2MemoReceiver");
    let mut sites = CastSiteCache::new();
    let referencing = ClassId::new(1);
    // Other tests move the process-wide epochs; retry for a quiet window.
    let mut filled = false;
    for _ in 0..200 {
        if crate::classloading::any_class_redefined() {
            return; // the latch disables every site cache by design
        }
        let site = CastSite {
            negative_receivers: [Some(receiver), None],
            ..CastSite::resolved(target).observed_at(cast_memo_epoch_now())
        };
        sites.put(referencing, 7, CastSiteCache::epochs_now(), site);
        if sites.get(referencing, 7).copied().map(CastSite::observed) == Some(site) {
            filled = true;
            break;
        }
    }
    if !filled {
        return;
    }
    // `receiver` gains `target` as its superclass: the stored refusal is
    // now wrong and must not be served.
    shared
        .classes
        .class_manager
        .write()
        .set_superclass(receiver, Some(target));
    let observed = sites.get(referencing, 7).copied().map(CastSite::observed);
    assert!(!observed.is_some_and(|site| site.refuses(receiver)));
    assert!(class_is_subtype(&shared, receiver, target));
}
