use super::is_string_builder_layout_native_override;

/// Every builder operation whose real JDK body indexes the compact
/// `byte[] value` / `byte coder` / `int count` layout must resolve through
/// CratonVM's native shim, because CratonVM's builders are a two-field
/// `char[]`/`int` synthetic. Before 2026-07-31 six of these were missing,
/// and a single `Mockito.mock(StringBuilder.class)` anywhere in the process
/// evicted their native shadow and silently corrupted every REAL builder —
/// which is what wedged javac's `JavaTokenizer` and made Spring's AOT
/// chunk 4 look like a hang. `SbMethodMatrixProbe` is the Java witness.
#[test]
fn forces_every_compact_layout_operation_native() {
    for class in ["java/lang/StringBuilder", "java/lang/AbstractStringBuilder"] {
        for (name, desc) in [
            ("setLength", "(I)V"),
            ("deleteCharAt", "(I)Ljava/lang/StringBuilder;"),
            ("replace", "(IILjava/lang/String;)Ljava/lang/StringBuilder;"),
            ("ensureCapacity", "(I)V"),
            ("trimToSize", "()V"),
            ("repeat", "(II)Ljava/lang/StringBuilder;"),
            ("capacity", "()I"),
            ("reverse", "()Ljava/lang/StringBuilder;"),
            ("codePointAt", "(I)I"),
            ("codePointBefore", "(I)I"),
            ("codePointCount", "(II)I"),
            ("getCoder", "()B"),
            ("getValue", "()[B"),
            // Already covered before this fix; kept so a future narrowing
            // of the list is caught here too.
            ("<init>", "(Ljava/lang/String;)V"),
            ("append", "(C)Ljava/lang/StringBuilder;"),
            ("charAt", "(I)C"),
            ("delete", "(II)Ljava/lang/StringBuilder;"),
            ("getChars", "(II[CI)V"),
            ("insert", "(IC)Ljava/lang/StringBuilder;"),
            ("setCharAt", "(IC)V"),
            ("toString", "()Ljava/lang/String;"),
            ("substring", "(II)Ljava/lang/String;"),
        ] {
            assert!(
                is_string_builder_layout_native_override(class, name, desc),
                "{class}.{name}{desc} must stay forced to its native shim"
            );
        }
    }
}

/// The two operations `MockitoBeanByTypeLookupIntegrationTests` genuinely
/// stubs and verifies on a mocked `StringBuilder`. Their native shadow has
/// to stay evictable or Mockito's woven advice never runs and the stub is
/// silently ignored — see the long note on `length()` in the predicate.
#[test]
fn leaves_the_two_mockito_stubbed_operations_evictable() {
    for class in ["java/lang/StringBuilder", "java/lang/AbstractStringBuilder"] {
        assert!(!is_string_builder_layout_native_override(
            class, "length", "()I"
        ));
        assert!(!is_string_builder_layout_native_override(
            class,
            "substring",
            "(I)Ljava/lang/String;"
        ));
    }
}

/// `java/lang/StringBuffer` is claimed by NOTHING here, and that is the
/// paired half of retiring its 62 registrations: its own `synchronized`
/// bodies must run, or the buffer loses both its monitor and its
/// `toStringCache` invalidation. Asserted operation by operation rather
/// than once, so a future widening of the list cannot quietly re-take the
/// class.
#[test]
fn never_claims_string_buffer_whose_own_bodies_carry_the_monitor() {
    for (name, desc) in [
        ("append", "(Ljava/lang/String;)Ljava/lang/StringBuffer;"),
        ("append", "(C)Ljava/lang/StringBuffer;"),
        ("insert", "(ILjava/lang/String;)Ljava/lang/StringBuffer;"),
        ("delete", "(II)Ljava/lang/StringBuffer;"),
        ("deleteCharAt", "(I)Ljava/lang/StringBuffer;"),
        ("replace", "(IILjava/lang/String;)Ljava/lang/StringBuffer;"),
        ("reverse", "()Ljava/lang/StringBuffer;"),
        ("setLength", "(I)V"),
        ("setCharAt", "(IC)V"),
        ("toString", "()Ljava/lang/String;"),
        ("charAt", "(I)C"),
        ("length", "()I"),
        ("capacity", "()I"),
        ("ensureCapacity", "(I)V"),
        ("trimToSize", "()V"),
        ("getChars", "(II[CI)V"),
        ("substring", "(II)Ljava/lang/String;"),
        ("<init>", "(Ljava/lang/String;)V"),
    ] {
        assert!(
            !is_string_builder_layout_native_override("java/lang/StringBuffer", name, desc),
            "StringBuffer.{name}{desc} must run its OWN synchronized body"
        );
    }
}

#[test]
fn does_not_claim_unrelated_classes() {
    assert!(!is_string_builder_layout_native_override(
        "java/lang/String",
        "setLength",
        "(I)V"
    ));
    assert!(!is_string_builder_layout_native_override(
        "java/util/ArrayList",
        "trimToSize",
        "()V"
    ));
}
