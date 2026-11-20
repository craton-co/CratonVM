use super::*;

const CLONE: &str = "()Ljava/lang/Object;";

fn helper(class: &str, method: &str, desc: &str, admits: bool) -> usize {
    let table = DirectHelperTable {
        object_clone: 0x0c10,
        ..DirectHelperTable::EMPTY
    };
    let admission = move |_: &str, _: &str, _: &str| admits;
    object_clone_helper(&table, true, Some(&admission), class, method, desc)
}

/// `Object.clone()` itself binds; a subclass's or an array's `clone`, and
/// another `Object` method, do not; the registry's refusal is honoured.
#[test]
fn only_object_clone_itself_binds() {
    assert_eq!(helper("java/lang/Object", "clone", CLONE, true), 0x0c10);
    assert_eq!(helper("java/util/TimeZone", "clone", CLONE, true), 0);
    assert_eq!(helper("[B", "clone", CLONE, true), 0);
    assert_eq!(helper("java/lang/Object", "hashCode", "()I", true), 0);
    assert_eq!(helper("java/lang/Object", "clone", CLONE, false), 0);
}

