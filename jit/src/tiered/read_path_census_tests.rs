use super::*;

/// i73-L5 (lane zip10): the read path's classes match, nested classes of
/// an exact-class prefix match, a longer sibling name does not.
#[test]
fn read_path_classes_match_by_package_or_exact_class() {
    for c in [
        "java/util/zip/ZipFile",
        "java/util/zip/ZipFile$Source",
        "java/util/zip/ZipUtils",
        "java/util/jar/JarFile",
        "jdk/internal/loader/URLClassPath$JarLoader",
        "java/net/URL",
        "java/net/URLClassLoader",
        "sun/net/www/ParseUtil",
        "jdk/internal/util/Preconditions",
    ] {
        assert!(read_path_census_class(c), "{c}");
    }
    for c in [
        "java/net/URLConnection",
        "java/net/URI",
        "java/util/HashMap",
        "sun/net/www/ParseUtils",
        "java/util/zipper/X",
    ] {
        assert!(!read_path_census_class(c), "{c}");
    }
}

#[test]
fn watch_patterns_split_and_bare_on_words_mean_default() {
    assert!(watch_patterns_of("1").is_empty());
    assert!(watch_patterns_of(" TRUE ").is_empty());
    let p = watch_patterns_of("javac/code/Types.isSameType, ,Attr.attribClass");
    assert_eq!(p, ["javac/code/Types.isSameType", "Attr.attribClass"]);
    assert!(watched(&p, "com/sun/tools/javac/code/Types", "isSameType"));
    assert!(!watched(&p, "com/sun/tools/javac/code/Types", "isSubtype"));
    assert!(!watched(&[], "a", "b"));
}

/// The census lines: one summary, the top rows by invocations, fields as
/// `key=value` and the name last; nothing for no rows.
#[test]
fn read_path_census_lines_rank_by_invocations() {
    assert!(read_path_census_lines(Vec::new()).is_empty());
    let row = |n: u64, tier, deopts, fails, inelig, name: &str, reason: &str| -> ReadPathRow {
        (
            n,
            tier,
            deopts,
            fails,
            inelig,
            false,
            name.to_string(),
            reason.to_string(),
        )
    };
    let lines = read_path_census_lines(vec![
        row(
            10,
            CompilationTier::C2,
            1,
            0,
            false,
            "java/util/zip/ZipFile.getEntry(Ljava/lang/String;)Ljava/util/zip/ZipEntry;",
            "-",
        ),
        row(
            900,
            CompilationTier::Interpreter,
            0,
            2,
            false,
            "java/util/zip/ZipUtils.get16([BI)I",
            "monitor here",
        ),
        row(
            50,
            CompilationTier::C1,
            0,
            0,
            false,
            "java/net/URL.<init>(Ljava/net/URL;Ljava/lang/String;)V",
            "-",
        ),
    ]);
    assert_eq!(lines.len(), 4);
    assert_eq!(
        lines[0],
        "[cratonvm] jit-read-path-census summary methods=3 interpreted=1 compiled=2 \
         refused=1 deopted=1 interpreted-invocations=900 total-invocations=960"
    );
    assert_eq!(
        lines[1],
        "[cratonvm] jit-read-path-census rank=1 inv=900 tier=Interpreter deopts=0 fails=2 \
         ineligible=false queued=false reason=monitor_here java/util/zip/ZipUtils.get16([BI)I"
    );
    assert!(lines[2].contains("rank=2 inv=50 tier=C1 "), "{}", lines[2]);
    assert!(lines[3].ends_with(
        "java/util/zip/ZipFile.getEntry(Ljava/lang/String;)Ljava/util/zip/ZipEntry;"
    ));
}
