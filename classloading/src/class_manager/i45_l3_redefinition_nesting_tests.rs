use super::*;

fn nesting(
    host: Option<&str>,
    members: &[&str],
    components: &[(&str, &str)],
    permitted: &[&str],
) -> RedefinitionNesting {
    RedefinitionNesting {
        nest_host: host.map(str::to_string),
        nest_members: members.iter().map(|s| s.to_string()).collect(),
        record_components: components
            .iter()
            .map(|(n, d)| (n.to_string(), d.to_string()))
            .collect(),
        permitted_subclasses: permitted.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn the_nest_host_must_name_the_same_class() {
    let member = nesting(Some("p/H"), &[], &[], &[]);
    assert!(member
        .changed_by(&nesting(Some("p/H"), &[], &[], &[]))
        .is_none());
    assert!(member.changed_by(&nesting(None, &[], &[], &[])).is_some());
    assert!(member
        .changed_by(&nesting(Some("p/X"), &[], &[], &[]))
        .is_some());
    assert!(nesting(None, &[], &[], &[])
        .changed_by(&nesting(Some("p/H"), &[], &[], &[]))
        .is_some());
}

#[test]
fn members_and_permitted_subclasses_are_sets() {
    let host = nesting(None, &["p/H$A", "p/H$B"], &[], &["p/H$A", "p/H$B"]);
    let reordered = nesting(None, &["p/H$B", "p/H$A"], &[], &["p/H$B", "p/H$A"]);
    assert!(host.changed_by(&reordered).is_none());
    assert!(host
        .changed_by(&nesting(None, &["p/H$A"], &[], &["p/H$A", "p/H$B"]))
        .is_some());
    assert!(host
        .changed_by(&nesting(
            None,
            &["p/H$A", "p/H$B"],
            &[],
            &["p/H$A", "p/H$C"]
        ))
        .is_some());
}

#[test]
fn record_components_keep_their_order_names_and_descriptors() {
    let record = nesting(None, &[], &[("x", "I"), ("y", "I")], &[]);
    assert!(record
        .changed_by(&nesting(None, &[], &[("x", "I"), ("y", "I")], &[]))
        .is_none());
    assert!(record
        .changed_by(&nesting(None, &[], &[("y", "I"), ("x", "I")], &[]))
        .is_some());
    assert!(record
        .changed_by(&nesting(None, &[], &[("x", "J"), ("y", "I")], &[]))
        .is_some());
    assert!(record
        .changed_by(&nesting(None, &[], &[("x", "I")], &[]))
        .is_some());
}

#[test]
fn the_attribute_refusal_is_hotspots() {
    let (class, message) = RedefinitionRefusal::ClassAttributeChanged.instrument_throwable();
    assert_eq!(class, "java/lang/UnsupportedOperationException");
    assert!(message.is_some_and(|m| m.contains("NestHost, NestMembers, Record")));
}
