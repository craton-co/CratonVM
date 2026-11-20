use super::*;
use crate::retired_shadow::{triple_is_retired_shadow, RETIRED_SHADOW_TABLES};

fn rules(raw: &str) -> Vec<Rule> {
    build(raw, false).map(|a| a.rules).unwrap_or_default()
}

#[test]
fn the_default_is_inert_across_every_retired_row() {
    // Not a sample. The whole population, because this switch is compiled
    // into every shipping binary and a default that is *nearly* inert is a
    // defect in every lane at once.
    let mut rows = 0;
    for table in RETIRED_SHADOW_TABLES {
        for (c, m, d) in table.iter() {
            assert!(
                !is_excluded(c, m, d),
                "{c}.{m}{d} is excluded with the variable unset — the \
                 default is not inert"
            );
            assert!(
                triple_is_retired_shadow(c, m, d),
                "{c}.{m}{d} stopped being retired with the variable unset"
            );
            rows += 1;
        }
    }
    assert!(
        rows > 500,
        "expected the tables to be populated, saw {rows}"
    );
    assert!(report().is_empty());
}

#[test]
fn pure_bytecode_dial_kind_rule_is_inert_off_and_yields_intrinsics_on() {
    use crate::NativeKind;
    // Lever off (every shipping configuration): the historical rule.
    assert!(dial_kind_decision(NativeKind::Bridge, false));
    assert!(!dial_kind_decision(NativeKind::Intrinsic, false));
    assert!(!dial_kind_decision(NativeKind::SyntheticStub, false));
    // Lever on: Bridge and reviewed Intrinsic yield; a stub never does
    // (it is refused, not declined).
    assert!(dial_kind_decision(NativeKind::Bridge, true));
    assert!(dial_kind_decision(NativeKind::Intrinsic, true));
    assert!(!dial_kind_decision(NativeKind::SyntheticStub, true));
    // Unset in the test process: the lever itself reads false.
    assert!(!pure_bytecode());
}

#[test]
fn each_shape_parses_to_the_rule_it_reads_as() {
    assert_eq!(rules("all"), vec![Rule::All]);
    assert_eq!(rules("ALL"), vec![Rule::All]);
    assert_eq!(rules("java/io/"), vec![Rule::Prefix("java/io/".into())]);
    assert_eq!(
        rules("java/io/File"),
        vec![Rule::Class("java/io/File".into())]
    );
    assert_eq!(
        rules("java/io/File.isAbsolute"),
        vec![Rule::ClassMethod(
            "java/io/File".into(),
            "isAbsolute".into()
        )]
    );
    assert_eq!(
        rules("java/io/File.isAbsolute()Z"),
        vec![Rule::Triple(
            "java/io/File".into(),
            "isAbsolute".into(),
            "()Z".into()
        )]
    );
}

#[test]
fn a_descriptor_with_slashes_and_parens_survives_parsing() {
    // The grammar claim in `parse_rule` is that the first `(` ends the
    // head and the last `.` before it splits class from method. A
    // descriptor full of `/` and `;` is where that claim earns its keep.
    assert_eq!(
        rules("java/io/File.listFiles()[Ljava/io/File;"),
        vec![Rule::Triple(
            "java/io/File".into(),
            "listFiles".into(),
            "()[Ljava/io/File;".into()
        )]
    );
    assert_eq!(
        rules("java/io/File.<init>(Ljava/lang/String;)V"),
        vec![Rule::Triple(
            "java/io/File".into(),
            "<init>".into(),
            "(Ljava/lang/String;)V".into()
        )]
    );
}

#[test]
fn a_list_is_split_and_blanks_are_dropped() {
    assert_eq!(
        rules(" java/io/ , , java/nio/ "),
        vec![
            Rule::Prefix("java/io/".into()),
            Rule::Prefix("java/nio/".into())
        ]
    );
}

#[test]
fn an_unparseable_entry_is_rejected_rather_than_guessed() {
    assert_eq!(parse_rule("()V"), None);
    assert_eq!(parse_rule(".foo"), None);
    assert_eq!(parse_rule("java/io/File."), None);
    assert_eq!(parse_rule(""), None);
}

#[test]
fn a_rule_reports_the_rows_it_reaches_and_zero_is_visible() {
    // The whole point of resolving at arm time: a plausible-looking rule
    // that reaches nothing is reported as 0 rather than silently
    // exonerating whatever it was meant to name.
    let armed = build("java/io/File,java/io/Reader.close,all", false).unwrap();
    let by_text: Vec<(String, usize)> = armed.report;
    assert_eq!(by_text[0].0, "java/io/File");
    assert!(
        by_text[0].1 >= 50,
        "java/io/File should reach its retired rows, got {}",
        by_text[0].1
    );
    assert_eq!(
        by_text[1].1, 0,
        "java/io/Reader.close is in no table; it must report 0"
    );
    assert!(by_text[2].1 > by_text[0].1, "`all` must reach every row");
}

#[test]
fn matching_is_scoped_exactly_as_written() {
    let c = Rule::Class("java/io/File".into());
    assert!(c.matches("java/io/File", "isAbsolute", "()Z"));
    assert!(!c.matches("java/io/FileWriter", "close", "()V"));

    let p = Rule::Prefix("java/io/".into());
    assert!(p.matches("java/io/FileWriter", "close", "()V"));
    assert!(!p.matches("java/nio/ByteBuffer", "limit", "()I"));

    let t = Rule::Triple("java/io/File".into(), "isAbsolute".into(), "()Z".into());
    assert!(t.matches("java/io/File", "isAbsolute", "()Z"));
    assert!(!t.matches("java/io/File", "isAbsolute", "()I"));
    assert!(!t.matches("java/io/File", "isDirectory", "()Z"));
}

/// The lever's exemptions are the documented ones and nothing wider: the
/// seven `Math` stubs, the VM-modelled families, and (Windows) the channel
/// layer. Everything else still yields to bytecode.
#[test]
fn the_lever_exemptions_are_exactly_the_documented_ones() {
    for m in ["pow", "sin", "cos", "tan", "exp", "log", "log10"] {
        assert!(lever_keeps_hotspot_intrinsic("java/lang/Math", m), "{m}");
    }
    assert!(!lever_keeps_hotspot_intrinsic("java/lang/Math", "abs"));
    assert!(!lever_keeps_hotspot_intrinsic(
        "java/lang/StrictMath",
        "pow"
    ));
    for c in [
        "java/lang/ref/WeakReference",
        "java/lang/ref/Reference",
        "jdk/jfr/internal/JVM",
        "java/lang/foreign/MemorySegment",
        "jdk/internal/foreign/MemorySessionImpl",
        "sun/awt/X11GraphicsEnvironment",
    ] {
        assert!(lever_keeps_platform_native(c), "{c}");
    }
    // SBR3 lang: measured droppable on Linux.
    for c in [
        "java/lang/ref/ReferenceQueue",
        "java/lang/ref/Cleaner",
        "java/lang/ref/SoftReference",
        "java/lang/ref/PhantomReference",
        "java/lang/ref/Finalizer",
        "jdk/internal/foreign/abi/Binding",
        "jdk/jfr/events/FileReadEvent",
    ] {
        assert!(!lever_keeps_platform_native(c), "{c}");
    }
    assert_eq!(
        lever_keeps_platform_native("jdk/internal/module/ModuleReferenceImpl"),
        cfg!(windows)
    );
    assert!(!lever_keeps_platform_native("java/lang/reflect/Method"));
    assert!(!lever_keeps_platform_native(
        "java/lang/reflect/Constructor"
    ));
    assert!(!lever_keeps_platform_native("java/util/HashMap"));
    assert!(!lever_keeps_platform_native("java/security/SecureRandom"));
    // Windows only: no real native layer under these.
    assert_eq!(
        lever_keeps_platform_native("sun/nio/ch/WEPollSelectorImpl"),
        cfg!(windows)
    );
    assert_eq!(
        lever_keeps_hotspot_intrinsic("sun/nio/fs/WindowsFileSystemProvider", "checkAccess"),
        cfg!(windows)
    );
    assert!(!lever_keeps_hotspot_intrinsic(
        "sun/nio/fs/WindowsFileSystemProvider",
        "readAttributes"
    ));
}
