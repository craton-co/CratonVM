use super::single_pass_only_filter_matches as m;

#[test]
fn an_empty_or_blank_filter_matches_nothing() {
    assert!(!m("", "Zs5Bt2Stores", "burstPut"));
    assert!(!m(" , ,", "Zs5Bt2Stores", "burstPut"));
}

#[test]
fn an_entry_matches_a_substring_of_class_dot_method() {
    assert!(m("Zs5Bt2Stores.burstPut", "Zs5Bt2Stores", "burstPut"));
    assert!(!m("Zs5Bt2Stores.burstPut", "Zs5Bt2Stores", "burstVhSet"));
    // A class-wide entry.
    assert!(m("Zs5Bt2Stores.", "Zs5Bt2Stores", "cost"));
    // Internal names, as CRATONVM_JIT_DENY.
    assert!(m("java/util/HashMap.put", "java/util/HashMap", "putVal"));
    assert!(!m("java.util.HashMap.put", "java/util/HashMap", "putVal"));
}

#[test]
fn any_entry_of_a_list_matches_and_entries_are_trimmed() {
    let f = "Nope.x, Zs5Bt2Stores.burstPut ,Other.y";
    assert!(m(f, "Zs5Bt2Stores", "burstPut"));
    assert!(m(f, "Other", "y"));
    assert!(!m(f, "Zs5Bt2Stores", "burstFresh"));
}
