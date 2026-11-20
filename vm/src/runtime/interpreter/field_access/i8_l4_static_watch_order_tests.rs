/// JVMTI `FieldAccess` / `FieldModification` for a static field fire AFTER
/// the holder's initialization, as HotSpot posts them (its `getstatic` /
/// `putstatic` templates resolve the field, which initializes the holder,
/// and only then post): a `<clinit>` that throws produces no event. Until
/// interpreter round i1 wave 8 both handlers fired first. A source witness,
/// because the alternative is a JVMTI agent plus a class whose initializer
/// throws, for an ORDERING claim about two statements.
#[test]
fn static_field_watch_events_fire_after_the_holder_is_initialized() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/runtime/interpreter/opcodes.rs"
    ))
    .expect("opcodes.rs is readable")
    .replace("\r\n", "\n");
    let body = |sig: &str| -> String {
        let start = src.find(sig).unwrap_or_else(|| panic!("{sig} not found"));
        let rest = &src[start..];
        // `\u{7d}` is a closing brace, spelled so this line's braces
        // balance for the brace-counting production-panic scanner.
        let stop = rest
            .find("\n\u{7d}\n")
            .unwrap_or_else(|| panic!("end of {sig} not found"));
        rest[..stop].to_string()
    };

    let put = body("pub(super) fn op_putstatic(");
    let init = put
        .find("ensure_class_initialized_shared(")
        .expect("op_putstatic initializes the holder");
    let fire = put
        .find("fire_field_modification_if_watched_for_vm(")
        .expect("op_putstatic fires the FieldModification watchpoint");
    assert!(
        init < fire,
        "putstatic must initialize the holder before the watch event"
    );

    let get = body("pub(super) fn op_getstatic(");
    let init = get
        .find("ensure_class_initialized_shared(")
        .expect("op_getstatic initializes the holder on its ordinary arm");
    let after_init = &get[init..];
    let fire = after_init
        .find("fire_access_watch(")
        .expect("op_getstatic fires the FieldAccess watchpoint after initializing");
    let read = after_init
        .find("get_static_shared(")
        .expect("op_getstatic reads the static after initializing");
    assert!(
        fire < read,
        "getstatic must fire the watch event before the read"
    );
}
