use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// The proper classpath-walking ServiceLoader natives are reachable
/// after `register_jdbc_driver_natives` runs. Catches a regression
/// where the registration was dropped from `lib.rs`.
///
/// SYNTHETIC-JDK ONLY since the 2026-08-29 retirement. `java.util.ServiceLoader`
/// is pure Java, and in a real-JDK build the VM now runs it: this
/// registrar's whole reason — keeping `register_p63_service_loader`'s
/// empty-iterator stubs from winning — is a synthetic-jdk concern, because
/// those stubs live in `register_synthetic_overrides` and exist nowhere
/// else. The real-JDK half of the contract is asserted by
/// `service_loader_is_left_to_the_jdk_in_a_real_jdk_build` below.
/// The retirement itself: in a real-JDK build NO `java/util/ServiceLoader`
/// method is registered, so the VM runs the JDK's own bytecode.
///
/// This is the half that had no test. `java.util.ServiceLoader` is pure
/// Java and the stub it replaced got the iterator's TYPE wrong --
/// `java.util.ArrayList$Itr` where HotSpot answers `java.util.ServiceLoader$2`
/// (MEASURED, `probes/DodServiceLoaderSweep`) -- and with it the lazy
/// iterator's semantics. `--real-jdk` has been refusing these
/// SyntheticStubs all along, which is how the bytecode path is known to
/// work: five definition-of-done workloads complete on it, `jdbc` 92/92 and
/// `h2jdbc` 12/12 among them, and those are `DriverManager` discovery --
/// the exact case this file was written for.
///
/// The JDBC-side helpers stay: they are this registrar's own natives, not a
/// shadow over anything the JDK provides.
///
/// 2026-09-05: this was `#[cfg(not(feature = "synthetic-jdk"))]` and read a
/// bare registry. Both halves moved: the retirement is now keyed on the
/// RUN's JDK mode (`drops_real_layout_synthetic`), so the test says which
/// mode it is asking about instead of inheriting it from the build, and it
/// compiles in both.
#[test]
fn service_loader_is_left_to_the_jdk_in_a_real_jdk_build() {
    let mut r = NativeMethodRegistry::new();
    r.set_drop_real_layout_synthetic(true);
    register_jdbc_driver_natives(&mut r);
    for (name, desc) in [
        ("load", "(Ljava/lang/Class;)Ljava/util/ServiceLoader;"),
        (
            "load",
            "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;",
        ),
        (
            "loadInstalled",
            "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        ),
        ("iterator", "()Ljava/util/Iterator;"),
        ("forEach", "(Ljava/util/function/Consumer;)V"),
        ("stream", "()Ljava/util/stream/Stream;"),
        ("spliterator", "()Ljava/util/Spliterator;"),
        ("findFirst", "()Ljava/util/Optional;"),
        ("reload", "()V"),
    ] {
        assert!(
            r.find("java/util/ServiceLoader", name, desc).is_none(),
            "java/util/ServiceLoader.{name}{desc} is registered in a real-JDK \
             build; it was retired on 2026-08-29 because the JDK's own \
             bytecode is what should answer it"
        );
    }
    assert!(r
        .find(
            "cratonvm/Wp71JdbcSpi",
            "findDriverProviderNative",
            "(Ljava/lang/String;)I",
        )
        .is_some());
}

/// The synthetic half of the same contract, and the reason the pair is
/// worth keeping: a change that dropped the rows in BOTH modes would
/// satisfy the real-JDK test above on its own.
///
/// 2026-09-05: was `#[cfg(feature = "synthetic-jdk")]`; a bare registry is
/// synthetic by default, so it now runs in every build.
#[test]
fn driver_natives_registered() {
    let mut r = NativeMethodRegistry::new();
    register_jdbc_driver_natives(&mut r);
    assert!(r
        .find(
            "java/util/ServiceLoader",
            "load",
            "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        )
        .is_some());
    assert!(r
        .find(
            "java/util/ServiceLoader",
            "iterator",
            "()Ljava/util/Iterator;"
        )
        .is_some());
    assert!(r
        .find(
            "java/util/ServiceLoader",
            "load",
            "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;",
        )
        .is_some());
    assert!(r
        .find(
            "cratonvm/Wp71JdbcSpi",
            "findDriverProviderNative",
            "(Ljava/lang/String;)I",
        )
        .is_some());
    assert!(r
        .find("cratonvm/Wp71JdbcSpi", "countDriverProvidersNative", "()I",)
        .is_some());
}

/// Plain text descriptor with two providers and a trailing comment.
#[test]
fn parse_provider_lines_strips_comments_and_blanks() {
    let body = b"# first comment\n  com.example.A  \n\ncom.example.B # trailing\n";
    let mut out = Vec::new();
    parse_provider_lines(body, &mut out);
    assert_eq!(out, vec!["com.example.A", "com.example.B"]);
}

/// Names with whitespace or non-identifier chars are dropped.
#[test]
fn parse_provider_lines_rejects_malformed() {
    let body = b"foo bar\n!bad\nGood$Inner\n";
    let mut out = Vec::new();
    parse_provider_lines(body, &mut out);
    assert_eq!(out, vec!["Good$Inner"]);
}
