use super::{class_name_is_proxy_super, simple_name_has_word};

/// The two names are decided by `CRATONVM_REAL_PROXY_SUPER`, which
/// defaults ON — so `java/lang/reflect/Proxy` is the arm a shipped
/// generated `$ProxyN` matches, and `Proxy$Instance` is the arm that
/// serves the opt-out, the degrade fallbacks and synthetic-JDK mode.
/// Dropping EITHER breaks a mode with no build error.
#[test]
fn both_proxy_supers_are_recognised_and_nothing_else_is() {
    assert!(
        class_name_is_proxy_super("java/lang/reflect/Proxy"),
        "the real base class is the DEFAULT super of every generated \
         $ProxyN (proxy_super_class_name(), real_proxy_super()=true); \
         dropping this arm recognises no proxy at all in the shipping \
         configuration"
    );
    assert!(
        class_name_is_proxy_super("java/lang/reflect/Proxy$Instance"),
        "the synthetic shim is still the allocated shape under \
         CRATONVM_REAL_PROXY_SUPER=0, under ProxyClassOutcome::Degrade \
         and ::Failed, and in synthetic-JDK mode"
    );
    // Not a prefix/suffix/contains test. A generated proxy is recognised
    // by its SUPER, never by its own name — `jdk/proxy1/$Proxy0` is a
    // measured JDK 25.0.3 proxy class name and must NOT match here, or
    // the chain walk would answer before it has walked anything.
    for name in [
        "jdk/proxy1/$Proxy0",
        "java/lang/reflect/ProxyGenerator",
        "java/lang/reflect/Proxy$ProxyBuilder",
        "java/lang/annotation/AnnotationProxy",
        "java/lang/Object",
        "",
    ] {
        assert!(
            !class_name_is_proxy_super(name),
            "{name} is not a proxy SUPERCLASS"
        );
    }
}

/// `simple_name_has_word` is `synthetic_implements`' whole collection
/// heuristic and had no in-tree assertion at all. Only OVER-ADMISSIONS
/// are defects (the function runs after `is_subclass_of` has declined, so
/// a `false` is "no opinion"), which is why the negative rows carry the
/// weight and the positive rows exist only to stop a
/// `fn(_,_) -> false` from passing.
///
/// **Every row's verdict is MEASURED on HotSpot 25.0.3+9-LTS**
/// (`X.class.isAssignableFrom(Y)` over `java.base`'s real nested class
/// names, enumerated with `getDeclaredClasses` rather than recalled).
///
/// **Mutation-checked**, by re-running this exact row set against four
/// hand-made variants of the function under plain `rustc`. Failing-row
/// counts, measured:
///
/// ```text
///   PRISTINE                                    0
///   drop the `ends_with("Iterator")` arm        5   (all `cursor` rows)
///   `simple` := the full `obj_name`             1   (ConcurrentSkipListMap$Values)
///   full name AND plain `contains` (historical) 7
///   drop the uppercase/digit word test          2   (`word` rows)
/// ```
///
/// The `1` is the honest number and worth keeping: the simple-name rule
/// and the word rule overlap almost completely, because a container name
/// usually ends in a lower-case letter (`…Collections$`) which the word
/// test already refuses. `ConcurrentSkipListMap$Values` is the one
/// measured row that separates them (`…SkipListMap` puts `List` in front
/// of a capital `M`), so deleting it would leave the simple-name rule
/// with no coverage of its own.
#[test]
fn a_container_never_lends_its_name_and_a_cursor_is_never_a_collection() {
    // 1. A container lends its name to every member — and must not
    //    (W8-C4-2, W8-C16-2). All MEASURED false on HotSpot.
    for (name, term) in [
        ("java/util/ImmutableCollections$Map1", "Collection"),
        ("java/util/ImmutableCollections$MapN", "Collection"),
        ("java/util/ImmutableCollections$StableMap", "Collection"),
        (
            "java/util/ImmutableCollections$StableMap$StableEntry",
            "Collection",
        ),
        ("java/util/concurrent/ConcurrentSkipListMap$Values", "List"),
        (
            "java/util/concurrent/ConcurrentSkipListMap$KeySpliterator",
            "List",
        ),
    ] {
        assert!(
            !simple_name_has_word(name, term),
            "{name} is not a {term} — its ENCLOSING class is"
        );
    }
    // 2. A cursor is not the thing it walks. Every row here has the term
    //    IN its simple name, so each one really does reach the iterator
    //    arm; a row like `ArrayList$Itr`/`List` would pass vacuously
    //    (simple name `Itr` contains no `List`) and is deliberately not
    //    used. All MEASURED false on HotSpot.
    for (name, term) in [
        ("java/util/ImmutableCollections$SetN$SetNIterator", "Set"),
        ("java/util/ImmutableCollections$ListItr", "List"),
        ("java/util/ArrayList$ListItr", "List"),
        ("java/util/LinkedList$ListItr", "List"),
        ("java/util/ArrayList$ArrayListSpliterator", "List"),
    ] {
        assert!(
            !simple_name_has_word(name, term),
            "{name} is a cursor OVER a {term}, not a {term}"
        );
    }
    // 3. "contains" is not "is". Both MEASURED false on HotSpot.
    assert!(
        !simple_name_has_word(
            "java/util/concurrent/locks/AbstractQueuedSynchronizer",
            "Queue"
        ),
        "AbstractQueuedSynchronizer contains \"Queue\" and is not one"
    );
    assert!(
        !simple_name_has_word("java/util/TooManyListenersException", "List"),
        "TooManyListenersException contains \"List\" and is not one"
    );
    // 4. The admissions that must SURVIVE, covering all three closing
    //    shapes the word test allows. All MEASURED true on HotSpot.
    assert!(
        simple_name_has_word("java/util/ArrayList", "List"),
        "the term ENDS the simple name"
    );
    assert!(
        simple_name_has_word(
            "java/util/ImmutableCollections$StableMap$StableMapEntrySet",
            "Set"
        ),
        "…EntrySet is a Set (measured true) and is nested two deep inside \
         a Map — the container rule must not swallow it"
    );
    assert!(
        simple_name_has_word("java/util/ImmutableCollections$SetN", "Set"),
        "the term is followed by another capitalised word"
    );
    assert!(
        simple_name_has_word("java/util/ImmutableCollections$List12", "List"),
        "a digit closes the word too"
    );
    assert!(simple_name_has_word("java/util/HashSet", "Set"));
    assert!(simple_name_has_word("java/util/ArrayDeque", "Deque"));
    // NOT asserted, and the reason is a correction to this function's own
    // doc comment: it cites `WorkQueue` and `ListResourceBundle` as
    // admissions the word rule is careful not to lose. MEASURED on
    // HotSpot 25.0.3+9, both are FALSE —
    // `Queue.class.isAssignableFrom(ForkJoinPool$WorkQueue)` and
    // `List.class.isAssignableFrom(ListResourceBundle)` are both `false`.
    // They are over-admissions this heuristic knowingly keeps (it can
    // only ADMIT), not correct admissions it preserves, so pinning them
    // green would pin the wrong claim. See
    // docs/feature-designs/real-jdk-mode.md §6.
}
