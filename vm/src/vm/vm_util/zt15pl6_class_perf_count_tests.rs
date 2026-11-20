/// zt t15/pl6: `sun.cls.linkedClasses` / `verifiedClasses` count each
/// link attempt at the `Loaded -> Verifying` edge, in both doors that make
/// it, and `sun.cls.initializedClasses` counts the `<clinit>` step once
/// the claim still holds (HotSpot's step 8, after the supertypes). A
/// source pin: the edges are the counts, and a third door that made the
/// edge without the count would undercount silently.
#[test]
fn zt15pl6_the_link_and_init_edges_carry_their_counts() {
    let src = include_str!("../vm_util.rs").replace("\r\n", "\n");
    let body = |head: &str| {
        let rest = src.split(head).nth(1).expect("function");
        rest.split("\n}\n").next().expect("body").to_string()
    };
    let edge = concat!("class.state = ClassState::", "Verifying;");
    for head in [
        "\npub fn link_class_without_java(",
        "\nfn link_claimed_class(",
    ] {
        let b = body(head);
        assert_eq!(b.matches(edge).count(), 1, "{head}: one edge");
        let count = concat!("note_class_link_begun", "(shared);");
        assert_eq!(b.matches(count).count(), 1, "{head}");
    }
    let init = body("\nfn initialize_class_shared(");
    let at = init
        .find("class.state = ClassState::Initializing;")
        .expect("step 4");
    let count = init.find(".classes_initialized").expect("the count");
    let claim = init
        .find("if !init_claim_held(class, current_thread_id)")
        .expect("claim");
    assert!(claim < at && at < count, "counted after the claim check, at the edge");
    // Every `Verifying` write outside the tests is one of the two doors.
    let code = src.split("\n#[cfg(test)]").next().expect("code");
    assert_eq!(code.matches(edge).count(), 2, "a new link door needs the count");
}
