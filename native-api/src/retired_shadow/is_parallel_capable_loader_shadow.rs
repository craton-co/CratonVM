#[allow(unused_imports)]
use super::*;

/// Lane T, wave 1 — the throwable family, retired whole: **906 triples over the
/// 62 classes of `THROWABLE_FAMILY_CLASSES`**, 2026-09-10.
///
/// A fourth table rather than entries merged into a sibling, for the reason the
/// second and third give for existing: this was adjudicated by a different
/// SCOPE, and the scope is the part worth seeing at a glance.
///
/// # Why the scope is the CLASS SET and not the registrar
///
/// The lane owns *cross-cutting registrars*, and the throwable family is
/// thirteen call sites in two files
/// (`native-builtins/src/lang_misc.rs::register_throwable_subclass_natives` and
/// `lib.rs::register_exception_extras_natives`) producing 872 of the lane's
/// 1,100 rows. But the instrument that priced this wave —
/// `CRATONVM_ENFORCE_NATIVE_SHADOW` — is keyed on the **receiver's class**, not
/// on the registering file, which the census records only as
/// `#[track_caller]` provenance. A registrar-scoped table would therefore retire
/// a set the arm never measured, and would leave a MIXTURE behind: 34 rows on
/// the same 62 classes come from a third registrar
/// (`reflect_annotations.rs`, `InvocationTargetException`) and from `lib.rs`
/// rows outside the two loops. `H0-4`'s standing finding is that
/// uniform-native works, uniform-bytecode works, and the mixture is the
/// configuration with evidence against it.
///
/// So the table is every LIVE (`owns_slot`) `Bridge` registration on those 62
/// classes whose image target carries a `Code` attribute, declared or
/// inherited — 906 of the 1,026 registered rows on them. The 120 that are not
/// here are not oversights:
///
/// ```text
///   115  superseded (owns_slot=false) -- covered anyway: the retirement is
///        per-TRIPLE and `register`'s re-tag fires at every ordinal
///     2  bucket D, ACC_NATIVE in the image -- §1.5 says `Bridge` is CORRECT:
///        Throwable.fillInStackTrace(I) and NullPointerException
///        .getExtendedNPEMessage()
///     2  bucket F, no such method on JDK 25 -- Throwable.getStackTraceDepth()
///        and .getStackTraceElement(I), a JDK 8 pair the image dropped
///     1  already SyntheticStub
/// ```
///
/// # What it costs and what it buys
///
/// Every one of the 1,021 registrations of these 906 triples is an ambient
/// `register()` under a `set_category(Bridge)` scope — `kind_stated: false`,
/// `kind_chosen: true` on all of them — so the re-tag in
/// [`NativeMethodRegistry::register`](crate::registry::NativeMethodRegistry::register)
/// reaches every one and there is no registration left to survive. That is the
/// prediction; the refusal/survivor count in the wave's record is the
/// measurement.
///
/// # The two defects the arm found, both of which had to be fixed FIRST
///
/// Armed over the 62 classes on the 64-probe promoted corpus, this wave started
/// at **10 differing lines against HotSpot across two probes**, from two root
/// causes — and both turned out to be present-tense defects in BOTH
/// compatibility modes rather than artefacts of the arming:
///
///  * `Throwable.fillInStackTrace()` reported ITSELF as the throw site. The
///    public no-arg form is real bytecode in every mode; only the private
///    `fillInStackTrace(int)` it calls is a native, and that native captured
///    the whole stack including the `Throwable.fillInStackTrace` frame above
///    it. HotSpot's `fill_in_stack_trace` skips two prefixes of the innermost
///    end — `fillInStackTrace*` frames, then `<init>` frames, in both cases
///    only where the throwable `is_a` the frame's holder — and
///    `stackwalker::trim_throwable_fill_frames` now does the same. The second
///    prefix is what makes the `<init>` rows in this table retirable at all:
///    once they yield, `Throwable.<init>` and its whole `super(...)` chain are
///    ordinary Java frames sitting on top of the throw site.
///  * `String[] a = null; a.clone()` answered `NullPointerException: clone on
///    null` where HotSpot names the array type and the null expression. An
///    array-typed call site is dispatched by a branch chosen BEFORE the
///    interpreter's null-receiver arm, so the JEP 358 message was never built
///    and `Object.clone`'s forced native — which has no bytecode context to
///    name an expression from — answered instead.
///
/// `regression-suite/src/RThrowableFillFrames.java` is the vector for both.
/// Neither is visible without arming something: nothing in the tree called
/// `fillInStackTrace()` explicitly, and nothing cloned a null array.
pub(in crate::retired_shadow) static RETIRED_SHADOW_LT_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/io/EOFException", "<init>", "()V"),
    ("java/io/EOFException", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/io/EOFException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/io/EOFException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/io/EOFException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    ("java/io/EOFException", "getMessage", "()Ljava/lang/String;"),
    (
        "java/io/EOFException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/io/EOFException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/io/EOFException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/io/EOFException", "printStackTrace", "()V"),
    (
        "java/io/EOFException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    ("java/io/EOFException", "toString", "()Ljava/lang/String;"),
    ("java/io/FileNotFoundException", "<init>", "()V"),
    (
        "java/io/FileNotFoundException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/io/FileNotFoundException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/io/FileNotFoundException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/io/FileNotFoundException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/io/FileNotFoundException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/io/FileNotFoundException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/io/FileNotFoundException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/io/FileNotFoundException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/io/FileNotFoundException", "printStackTrace", "()V"),
    (
        "java/io/FileNotFoundException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/io/FileNotFoundException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/io/IOException", "<init>", "()V"),
    ("java/io/IOException", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/io/IOException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    ("java/io/IOException", "<init>", "(Ljava/lang/Throwable;)V"),
    (
        "java/io/IOException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    ("java/io/IOException", "getCause", "()Ljava/lang/Throwable;"),
    (
        "java/io/IOException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    ("java/io/IOException", "getMessage", "()Ljava/lang/String;"),
    (
        "java/io/IOException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/io/IOException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/io/IOException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/io/IOException", "printStackTrace", "()V"),
    (
        "java/io/IOException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    ("java/io/IOException", "toString", "()Ljava/lang/String;"),
    ("java/io/NotSerializableException", "<init>", "()V"),
    (
        "java/io/NotSerializableException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/io/NotSerializableException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/io/NotSerializableException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/io/NotSerializableException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/io/NotSerializableException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/io/NotSerializableException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/io/NotSerializableException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/io/NotSerializableException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/io/NotSerializableException", "printStackTrace", "()V"),
    (
        "java/io/NotSerializableException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/io/NotSerializableException",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/io/UncheckedIOException",
        "<init>",
        "(Ljava/io/IOException;)V",
    ),
    (
        "java/io/UncheckedIOException",
        "<init>",
        "(Ljava/lang/String;Ljava/io/IOException;)V",
    ),
    (
        "java/io/UncheckedIOException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/io/UncheckedIOException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/io/UncheckedIOException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/io/UncheckedIOException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/io/UncheckedIOException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/io/UncheckedIOException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/io/UncheckedIOException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/io/UncheckedIOException", "printStackTrace", "()V"),
    (
        "java/io/UncheckedIOException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/io/UncheckedIOException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/io/UnsupportedEncodingException", "<init>", "()V"),
    (
        "java/io/UnsupportedEncodingException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/io/UnsupportedEncodingException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/io/UnsupportedEncodingException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/io/UnsupportedEncodingException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/io/UnsupportedEncodingException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/io/UnsupportedEncodingException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/io/UnsupportedEncodingException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/io/UnsupportedEncodingException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/io/UnsupportedEncodingException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/io/UnsupportedEncodingException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/io/UnsupportedEncodingException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/AbstractMethodError", "<init>", "()V"),
    (
        "java/lang/AbstractMethodError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/AbstractMethodError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/AbstractMethodError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/AbstractMethodError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/AbstractMethodError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/AbstractMethodError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/AbstractMethodError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/AbstractMethodError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/AbstractMethodError", "printStackTrace", "()V"),
    (
        "java/lang/AbstractMethodError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/AbstractMethodError",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/ArithmeticException", "<init>", "()V"),
    (
        "java/lang/ArithmeticException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/ArithmeticException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/ArithmeticException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ArithmeticException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/ArithmeticException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/ArithmeticException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/ArithmeticException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ArithmeticException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/ArithmeticException", "printStackTrace", "()V"),
    (
        "java/lang/ArithmeticException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/ArithmeticException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/ArrayIndexOutOfBoundsException", "<init>", "()V"),
    ("java/lang/ArrayIndexOutOfBoundsException", "<init>", "(I)V"),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/AssertionError", "<init>", "()V"),
    ("java/lang/AssertionError", "<init>", "(C)V"),
    ("java/lang/AssertionError", "<init>", "(D)V"),
    ("java/lang/AssertionError", "<init>", "(F)V"),
    ("java/lang/AssertionError", "<init>", "(I)V"),
    ("java/lang/AssertionError", "<init>", "(J)V"),
    (
        "java/lang/AssertionError",
        "<init>",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/lang/AssertionError",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    ("java/lang/AssertionError", "<init>", "(Z)V"),
    (
        "java/lang/AssertionError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/AssertionError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/AssertionError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/AssertionError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/AssertionError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/AssertionError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/AssertionError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/AssertionError", "printStackTrace", "()V"),
    (
        "java/lang/AssertionError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/AssertionError",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/ClassCastException", "<init>", "()V"),
    (
        "java/lang/ClassCastException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/ClassCastException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/ClassCastException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ClassCastException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/ClassCastException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/ClassCastException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/ClassCastException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ClassCastException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/ClassCastException", "printStackTrace", "()V"),
    (
        "java/lang/ClassCastException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/ClassCastException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/ClassNotFoundException", "<init>", "()V"),
    (
        "java/lang/ClassNotFoundException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/ClassNotFoundException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/ClassNotFoundException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/ClassNotFoundException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ClassNotFoundException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/ClassNotFoundException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/ClassNotFoundException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/ClassNotFoundException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ClassNotFoundException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/ClassNotFoundException", "printStackTrace", "()V"),
    (
        "java/lang/ClassNotFoundException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/ClassNotFoundException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/CloneNotSupportedException", "<init>", "()V"),
    (
        "java/lang/CloneNotSupportedException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/Error", "<init>", "()V"),
    ("java/lang/Error", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/lang/Error",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    ("java/lang/Error", "<init>", "(Ljava/lang/Throwable;)V"),
    (
        "java/lang/Error",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    ("java/lang/Error", "getCause", "()Ljava/lang/Throwable;"),
    (
        "java/lang/Error",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    ("java/lang/Error", "getMessage", "()Ljava/lang/String;"),
    (
        "java/lang/Error",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/Error",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/Error",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/Error", "printStackTrace", "()V"),
    (
        "java/lang/Error",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    ("java/lang/Error", "toString", "()Ljava/lang/String;"),
    ("java/lang/Exception", "<init>", "()V"),
    ("java/lang/Exception", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/lang/Exception",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    ("java/lang/Exception", "<init>", "(Ljava/lang/Throwable;)V"),
    (
        "java/lang/Exception",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    ("java/lang/Exception", "getCause", "()Ljava/lang/Throwable;"),
    (
        "java/lang/Exception",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    ("java/lang/Exception", "getMessage", "()Ljava/lang/String;"),
    (
        "java/lang/Exception",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/Exception",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/Exception",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/Exception", "printStackTrace", "()V"),
    (
        "java/lang/Exception",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    ("java/lang/Exception", "toString", "()Ljava/lang/String;"),
    ("java/lang/ExceptionInInitializerError", "<init>", "()V"),
    (
        "java/lang/ExceptionInInitializerError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "printStackTrace",
        "()V",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/IllegalAccessError", "<init>", "()V"),
    (
        "java/lang/IllegalAccessError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/IllegalAccessError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/IllegalAccessError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IllegalAccessError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/IllegalAccessError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/IllegalAccessError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/IllegalAccessError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IllegalAccessError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/IllegalAccessError", "printStackTrace", "()V"),
    (
        "java/lang/IllegalAccessError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/IllegalAccessError",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/IllegalAccessException", "<init>", "()V"),
    (
        "java/lang/IllegalAccessException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/IllegalAccessException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/IllegalAccessException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IllegalAccessException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/IllegalAccessException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/IllegalAccessException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/IllegalAccessException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IllegalAccessException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/IllegalAccessException", "printStackTrace", "()V"),
    (
        "java/lang/IllegalAccessException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/IllegalAccessException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/IllegalArgumentException", "<init>", "()V"),
    (
        "java/lang/IllegalArgumentException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/IllegalArgumentException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/IllegalArgumentException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/IllegalArgumentException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/IllegalArgumentException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IllegalArgumentException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/IllegalArgumentException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/IllegalArgumentException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/IllegalArgumentException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IllegalArgumentException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IllegalArgumentException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/lang/IllegalArgumentException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/IllegalArgumentException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/IllegalStateException", "<init>", "()V"),
    (
        "java/lang/IllegalStateException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/IllegalStateException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/IllegalStateException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/IllegalStateException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/IllegalStateException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IllegalStateException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/IllegalStateException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/IllegalStateException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/IllegalStateException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IllegalStateException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/IllegalStateException", "printStackTrace", "()V"),
    (
        "java/lang/IllegalStateException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/IllegalStateException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/IncompatibleClassChangeError", "<init>", "()V"),
    (
        "java/lang/IncompatibleClassChangeError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/IncompatibleClassChangeError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/IncompatibleClassChangeError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IncompatibleClassChangeError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/IncompatibleClassChangeError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/IncompatibleClassChangeError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/IncompatibleClassChangeError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IncompatibleClassChangeError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IncompatibleClassChangeError",
        "printStackTrace",
        "()V",
    ),
    (
        "java/lang/IncompatibleClassChangeError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/IncompatibleClassChangeError",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/IndexOutOfBoundsException", "<init>", "()V"),
    ("java/lang/IndexOutOfBoundsException", "<init>", "(I)V"),
    ("java/lang/IndexOutOfBoundsException", "<init>", "(J)V"),
    (
        "java/lang/IndexOutOfBoundsException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/IndexOutOfBoundsException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/IndexOutOfBoundsException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IndexOutOfBoundsException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/IndexOutOfBoundsException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/IndexOutOfBoundsException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/IndexOutOfBoundsException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IndexOutOfBoundsException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/lang/IndexOutOfBoundsException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/lang/IndexOutOfBoundsException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/IndexOutOfBoundsException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/InstantiationException", "<init>", "()V"),
    (
        "java/lang/InstantiationException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/InstantiationException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/InstantiationException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/InstantiationException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/InstantiationException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/InstantiationException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/InstantiationException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/InstantiationException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/InstantiationException", "printStackTrace", "()V"),
    (
        "java/lang/InstantiationException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/InstantiationException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/InternalError", "<init>", "()V"),
    ("java/lang/InternalError", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/lang/InternalError",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/InternalError",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/InternalError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/InternalError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/InternalError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/InternalError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/InternalError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/InternalError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/InternalError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/InternalError", "printStackTrace", "()V"),
    (
        "java/lang/InternalError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/InternalError",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/InterruptedException", "<init>", "()V"),
    (
        "java/lang/InterruptedException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/InterruptedException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/InterruptedException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/InterruptedException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/InterruptedException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/InterruptedException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/InterruptedException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/InterruptedException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/InterruptedException", "printStackTrace", "()V"),
    (
        "java/lang/InterruptedException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/InterruptedException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/LinkageError", "<init>", "()V"),
    ("java/lang/LinkageError", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/lang/LinkageError",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/LinkageError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/LinkageError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/LinkageError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/LinkageError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/LinkageError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/LinkageError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/LinkageError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/LinkageError", "printStackTrace", "()V"),
    (
        "java/lang/LinkageError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    ("java/lang/LinkageError", "toString", "()Ljava/lang/String;"),
    (
        "java/lang/MatchException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/MatchException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/MatchException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/MatchException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/MatchException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/MatchException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/MatchException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/MatchException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/MatchException", "printStackTrace", "()V"),
    (
        "java/lang/MatchException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/MatchException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/NegativeArraySizeException", "<init>", "()V"),
    (
        "java/lang/NegativeArraySizeException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/NoClassDefFoundError", "<init>", "()V"),
    (
        "java/lang/NoClassDefFoundError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/NoClassDefFoundError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/NoClassDefFoundError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NoClassDefFoundError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NoClassDefFoundError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NoClassDefFoundError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/NoClassDefFoundError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NoClassDefFoundError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/NoClassDefFoundError", "printStackTrace", "()V"),
    (
        "java/lang/NoClassDefFoundError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/NoClassDefFoundError",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/NoSuchFieldError", "<init>", "()V"),
    (
        "java/lang/NoSuchFieldError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/NoSuchFieldError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/NoSuchFieldError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NoSuchFieldError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NoSuchFieldError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NoSuchFieldError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/NoSuchFieldError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NoSuchFieldError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/NoSuchFieldError", "printStackTrace", "()V"),
    (
        "java/lang/NoSuchFieldError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/NoSuchFieldError",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/NoSuchFieldException", "<init>", "()V"),
    (
        "java/lang/NoSuchFieldException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/NoSuchFieldException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/NoSuchFieldException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NoSuchFieldException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NoSuchFieldException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NoSuchFieldException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/NoSuchFieldException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NoSuchFieldException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/NoSuchFieldException", "printStackTrace", "()V"),
    (
        "java/lang/NoSuchFieldException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/NoSuchFieldException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/NoSuchMethodError", "<init>", "()V"),
    (
        "java/lang/NoSuchMethodError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/NoSuchMethodError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/NoSuchMethodError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NoSuchMethodError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NoSuchMethodError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NoSuchMethodError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/NoSuchMethodError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NoSuchMethodError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/NoSuchMethodError", "printStackTrace", "()V"),
    (
        "java/lang/NoSuchMethodError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/NoSuchMethodError",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/NoSuchMethodException", "<init>", "()V"),
    (
        "java/lang/NoSuchMethodException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/NoSuchMethodException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/NoSuchMethodException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NoSuchMethodException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NoSuchMethodException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NoSuchMethodException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/NoSuchMethodException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NoSuchMethodException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/NoSuchMethodException", "printStackTrace", "()V"),
    (
        "java/lang/NoSuchMethodException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/NoSuchMethodException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/NullPointerException", "<init>", "()V"),
    (
        "java/lang/NullPointerException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/NullPointerException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/NullPointerException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NullPointerException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NullPointerException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NullPointerException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/NullPointerException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NullPointerException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/NullPointerException", "printStackTrace", "()V"),
    (
        "java/lang/NullPointerException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/NullPointerException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/NumberFormatException", "<init>", "()V"),
    (
        "java/lang/NumberFormatException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/NumberFormatException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/NumberFormatException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NumberFormatException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NumberFormatException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/NumberFormatException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/NumberFormatException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/NumberFormatException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/NumberFormatException", "printStackTrace", "()V"),
    (
        "java/lang/NumberFormatException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/NumberFormatException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/OutOfMemoryError", "<init>", "()V"),
    (
        "java/lang/OutOfMemoryError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/OutOfMemoryError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/OutOfMemoryError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/OutOfMemoryError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/OutOfMemoryError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/OutOfMemoryError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/OutOfMemoryError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/OutOfMemoryError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/OutOfMemoryError", "printStackTrace", "()V"),
    (
        "java/lang/OutOfMemoryError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/OutOfMemoryError",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/ReflectiveOperationException", "<init>", "()V"),
    (
        "java/lang/ReflectiveOperationException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/ReflectiveOperationException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/RuntimeException", "<init>", "()V"),
    (
        "java/lang/RuntimeException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/RuntimeException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/RuntimeException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/RuntimeException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/RuntimeException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/RuntimeException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/RuntimeException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/RuntimeException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/RuntimeException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/RuntimeException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/RuntimeException", "printStackTrace", "()V"),
    (
        "java/lang/RuntimeException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/RuntimeException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/SecurityException", "<init>", "()V"),
    (
        "java/lang/SecurityException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/SecurityException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/SecurityException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/SecurityException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/SecurityException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/SecurityException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/SecurityException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/SecurityException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/SecurityException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/SecurityException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/SecurityException", "printStackTrace", "()V"),
    (
        "java/lang/SecurityException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/SecurityException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/StackOverflowError", "<init>", "()V"),
    (
        "java/lang/StackOverflowError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/StackOverflowError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/StackOverflowError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/StackOverflowError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/StackOverflowError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/StackOverflowError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/StackOverflowError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/StackOverflowError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/StackOverflowError", "printStackTrace", "()V"),
    (
        "java/lang/StackOverflowError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/StackOverflowError",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/StringIndexOutOfBoundsException", "<init>", "()V"),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "<init>",
        "(I)V",
    ),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/StringIndexOutOfBoundsException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/Throwable", "<init>", "()V"),
    ("java/lang/Throwable", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/lang/Throwable",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    ("java/lang/Throwable", "<init>", "(Ljava/lang/Throwable;)V"),
    (
        "java/lang/Throwable",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    ("java/lang/Throwable", "getCause", "()Ljava/lang/Throwable;"),
    (
        "java/lang/Throwable",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    ("java/lang/Throwable", "getMessage", "()Ljava/lang/String;"),
    (
        "java/lang/Throwable",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/Throwable",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/Throwable",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/Throwable", "printStackTrace", "()V"),
    (
        "java/lang/Throwable",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    ("java/lang/Throwable", "toString", "()Ljava/lang/String;"),
    (
        "java/lang/TypeNotPresentException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/TypeNotPresentException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/TypeNotPresentException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/TypeNotPresentException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/TypeNotPresentException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/TypeNotPresentException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/TypeNotPresentException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/TypeNotPresentException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/lang/TypeNotPresentException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/lang/TypeNotPresentException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/TypeNotPresentException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/UnsatisfiedLinkError", "<init>", "()V"),
    (
        "java/lang/UnsatisfiedLinkError",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/UnsatisfiedLinkError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/UnsatisfiedLinkError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/UnsatisfiedLinkError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/UnsatisfiedLinkError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/UnsatisfiedLinkError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/UnsatisfiedLinkError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/UnsatisfiedLinkError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/UnsatisfiedLinkError", "printStackTrace", "()V"),
    (
        "java/lang/UnsatisfiedLinkError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/UnsatisfiedLinkError",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/UnsupportedOperationException", "<init>", "()V"),
    (
        "java/lang/UnsupportedOperationException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/UnsupportedOperationException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/lang/VerifyError", "<init>", "()V"),
    ("java/lang/VerifyError", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/lang/VerifyError",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/VerifyError",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/VerifyError",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/VerifyError",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/VerifyError",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/VerifyError",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/VerifyError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/lang/VerifyError", "printStackTrace", "()V"),
    (
        "java/lang/VerifyError",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    ("java/lang/VerifyError", "toString", "()Ljava/lang/String;"),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "<init>",
        "()V",
    ),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/reflect/InaccessibleObjectException",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/reflect/InvocationTargetException",
        "<init>",
        "()V",
    ),
    (
        "java/lang/reflect/InvocationTargetException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/lang/reflect/InvocationTargetException",
        "<init>",
        "(Ljava/lang/Throwable;Ljava/lang/String;)V",
    ),
    (
        "java/lang/reflect/InvocationTargetException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/reflect/InvocationTargetException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/reflect/InvocationTargetException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/lang/reflect/InvocationTargetException",
        "getTargetException",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/lang/reflect/InvocationTargetException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/lang/reflect/InvocationTargetException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/lang/reflect/InvocationTargetException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/lang/reflect/InvocationTargetException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/net/MalformedURLException", "<init>", "()V"),
    (
        "java/net/MalformedURLException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/net/MalformedURLException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/net/MalformedURLException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/net/MalformedURLException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/net/MalformedURLException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/net/MalformedURLException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/net/MalformedURLException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/net/MalformedURLException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/net/MalformedURLException", "printStackTrace", "()V"),
    (
        "java/net/MalformedURLException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/net/MalformedURLException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/net/UnknownHostException", "<init>", "()V"),
    (
        "java/net/UnknownHostException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/net/UnknownHostException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/net/UnknownHostException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/net/UnknownHostException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/net/UnknownHostException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/net/UnknownHostException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/net/UnknownHostException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/net/UnknownHostException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/net/UnknownHostException", "printStackTrace", "()V"),
    (
        "java/net/UnknownHostException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/net/UnknownHostException",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/text/ParseException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/text/ParseException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/text/ParseException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/text/ParseException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/text/ParseException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/text/ParseException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/text/ParseException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/text/ParseException", "printStackTrace", "()V"),
    (
        "java/text/ParseException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/text/ParseException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/ConcurrentModificationException", "<init>", "()V"),
    (
        "java/util/ConcurrentModificationException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/util/ConcurrentModificationException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/ConcurrentModificationException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/ConcurrentModificationException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/ConcurrentModificationException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/ConcurrentModificationException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/ConcurrentModificationException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/ConcurrentModificationException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/util/ConcurrentModificationException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/util/ConcurrentModificationException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/util/ConcurrentModificationException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/util/ConcurrentModificationException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/util/ConcurrentModificationException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/FormatterClosedException", "<init>", "()V"),
    (
        "java/util/FormatterClosedException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/FormatterClosedException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/FormatterClosedException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/FormatterClosedException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/FormatterClosedException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/util/FormatterClosedException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/util/FormatterClosedException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/util/FormatterClosedException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/util/FormatterClosedException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/util/FormatterClosedException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/InputMismatchException", "<init>", "()V"),
    (
        "java/util/InputMismatchException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/util/InputMismatchException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/InputMismatchException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/InputMismatchException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/InputMismatchException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/InputMismatchException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/util/InputMismatchException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/util/InputMismatchException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/util/InputMismatchException", "printStackTrace", "()V"),
    (
        "java/util/InputMismatchException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/util/InputMismatchException",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/MissingResourceException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
    ),
    (
        "java/util/MissingResourceException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/MissingResourceException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/MissingResourceException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/MissingResourceException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/MissingResourceException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/util/MissingResourceException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/util/MissingResourceException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/util/MissingResourceException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/util/MissingResourceException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/util/MissingResourceException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/NoSuchElementException", "<init>", "()V"),
    (
        "java/util/NoSuchElementException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/util/NoSuchElementException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/NoSuchElementException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/NoSuchElementException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/NoSuchElementException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/NoSuchElementException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/NoSuchElementException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/NoSuchElementException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/util/NoSuchElementException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/util/NoSuchElementException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    ("java/util/NoSuchElementException", "printStackTrace", "()V"),
    (
        "java/util/NoSuchElementException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/util/NoSuchElementException",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "<init>",
        "()V",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/util/concurrent/BrokenBarrierException",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/CancellationException",
        "<init>",
        "()V",
    ),
    (
        "java/util/concurrent/CancellationException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/util/concurrent/CancellationException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/CancellationException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/CancellationException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/CancellationException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/CancellationException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/util/concurrent/CancellationException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/CancellationException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/CancellationException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/util/concurrent/CancellationException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/util/concurrent/CancellationException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/concurrent/CompletionException", "<init>", "()V"),
    (
        "java/util/concurrent/CompletionException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/util/concurrent/CompletionException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/CompletionException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/CompletionException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/CompletionException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/CompletionException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/CompletionException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/CompletionException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/util/concurrent/CompletionException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/CompletionException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/CompletionException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/util/concurrent/CompletionException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/util/concurrent/CompletionException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/concurrent/ExecutionException", "<init>", "()V"),
    (
        "java/util/concurrent/ExecutionException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/util/concurrent/ExecutionException",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "<init>",
        "()V",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/util/concurrent/RejectedExecutionException",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/concurrent/TimeoutException", "<init>", "()V"),
    (
        "java/util/concurrent/TimeoutException",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/util/concurrent/TimeoutException",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/TimeoutException",
        "getCause",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/TimeoutException",
        "getLocalizedMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/TimeoutException",
        "getMessage",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/concurrent/TimeoutException",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
    ),
    (
        "java/util/concurrent/TimeoutException",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/TimeoutException",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/TimeoutException",
        "printStackTrace",
        "()V",
    ),
    (
        "java/util/concurrent/TimeoutException",
        "printStackTrace",
        "(Ljava/io/PrintStream;)V",
    ),
    (
        "java/util/concurrent/TimeoutException",
        "toString",
        "()Ljava/lang/String;",
    ),
];

/// Lane 7's wave, 2026-09-10: the two triples that stop
/// `jdk/internal/loader/BuiltinClassLoader` from linking under `--real-jdk`.
///
/// # The measurement
///
/// `BuiltinClassLoader.<clinit>` is, in JDK 25 bytecode,
/// `if (!ClassLoader.registerAsParallelCapable()) throw new InternalError(...)`,
/// and it threw. `CRATONVM_DBG_CLINIT_FAIL=1` named the class and the exception
/// (the `NoClassDefFoundError` every consumer sees names only the consumer);
/// `L7ParallelCapableProbe.java` then named the broken link, with
/// no reflection and therefore no `--add-opens` requirement:
///
/// ```text
///                                            HotSpot  armed  unarmed
///   Direct       extends ClassLoader          true     true   true
///   UnderSecure  extends SecureClassLoader    true     FALSE  true
///   UnderUrl     extends URLClassLoader       true     FALSE  true
/// ```
///
/// `ParallelLoaders.register(c)` answers `loaderTypes.contains(c.getSuperclass())`,
/// so `BuiltinClassLoader` — whose superclass is `SecureClassLoader` — can only
/// register once `SecureClassLoader` has. It never did, and the two
/// registrations below are jointly why:
///
///  * `ClassLoader.registerAsParallelCapable()Z` was a native returning a
///    constant `Int(1)`. It never touched the real `ParallelLoaders.loaderTypes`
///    set, so every caller was told `true` while the SET stayed empty of
///    everything the JDK believed it had registered.
///  * `java/security/SecureClassLoader.<clinit>()V` was a **no-op native**
///    (S111r9, a `--real-jdk` Spring-Boot-launcher workaround). It runs during
///    the built-in loader allocation chain, i.e. from inside a native, where the
///    dispatch dial cannot see it — so no arm of
///    `CRATONVM_ENFORCE_NATIVE_SHADOW` could ever have yielded this row. Only a
///    registration-time refusal reaches it, which is what this table is.
///
/// That combination is the corrupt MIXTURE `scripts/jdk-only-blast-radius.sh`
/// caveat 4 describes rather than a partial retirement: half the chain kept the
/// native's fictional answer and half read the real set.
///
/// # Why retirement rather than a faithful native
///
/// The native's own comment said a faithful implementation was "not
/// implementable ... `NativeContext` exposes no caller-class / stack-walk
/// accessor". `NativeContext::frame_class_ids` has existed since the
/// `latestUserDefinedLoader` work, so that sentence is stale — but a native
/// mirroring `ParallelLoaders` would still have to keep a second copy of a JDK
/// set in step with the JDK's own, and §1.4's remedy for a shadow over concrete
/// bytecode is to yield to it. Both rows are bucket A (the image method
/// declares `Code`), so yielding has somewhere to go.
///
/// `Compatible` is untouched — a `SyntheticStub` registers and dispatches
/// normally there — which is what keeps the S111r9 workaround intact for the
/// `--real-jdk` fat-jar launchers it was written for.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L7_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/lang/ClassLoader", "registerAsParallelCapable", "()Z"),
    ("java/security/SecureClassLoader", "<clinit>", "()V"),
];

/// Is `(class, method, descriptor)` one of [`RETIRED_SHADOW_L7_TRIPLES`], the
/// two shadows that keep `ClassLoader$ParallelLoaders` empty? The registry
/// drops them under `Compatible` too when that mode runs the JDK's
/// parallel-capable loader model
/// (`NativeMethodRegistry::compat_parallel_capable_loaders`; interpreter round
/// i1 wave 48, lane L5). Two string compares at most per registration.
pub(crate) fn is_parallel_capable_loader_shadow(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    RETIRED_SHADOW_L7_TRIPLES
        .iter()
        .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor)
}

/// Is `(class, method, descriptor)` one of [`RETIRED_SHADOW_URLCLASSLOADER_TRIPLES`],
/// the `URLClassLoader` / `URLClassPath` stand-ins? The registry drops every
/// registration of them under `Compatible` over a real JDK with the
/// `JdkUrlClassLoaders` HotSpot-alignment row, whatever the registrar or
/// kind, so the JDK's bodies run there as under `JdkOnly` (interpreter round
/// i1 wave 52, lane L5). Only `java/net/URLClassLoader` and
/// `jdk/internal/loader/URLClassPath` registrations reach the table scan.
pub(crate) fn is_url_class_loader_shadow(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        class_name,
        "java/net/URLClassLoader" | "jdk/internal/loader/URLClassPath"
    ) && RETIRED_SHADOW_URLCLASSLOADER_TRIPLES
        .iter()
        .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor)
}

/// Is `(class, method, descriptor)` one of [`RETIRED_SHADOW_URL_ACCESSOR_TRIPLES`],
/// the eight `java.net.URL` accessor stand-ins? They go with the
/// `JdkUrlClassLoaders` row (the i50-L5 `--compatible` gates page, item 6's
/// last piece): under that row every URL the VM mints is the JDK
/// constructor's or has its fields written by name (`build_synthetic_url`,
/// `url_write_declared_field_defaults`), so the JDK's one-field bodies are the
/// answer, as under `JdkOnly` since the wave-24 retirement.
pub(crate) fn is_url_accessor_shadow(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/net/URL"
        && RETIRED_SHADOW_URL_ACCESSOR_TRIPLES
            .iter()
            .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor)
}

/// Is `(class, method, descriptor)` one of the `java.io.File` family's
/// retired rows -- `File`'s own (`RETIRED_SHADOW_L4_TRIPLES`'s `File` rows and
/// `RETIRED_SHADOW_L4_FILETMP_TRIPLES`) and the two platform `FileSystem`
/// classes' Java wrappers (`RETIRED_SHADOW_L4_WINNTFS_TRIPLES`,
/// `RETIRED_SHADOW_L4_UNIXFS_TRIPLES`), which `--real-jdk` retired together?
/// The registry drops every registration of them under `Compatible` over a
/// real JDK with the `JdkFileBodies` HotSpot-alignment row (interpreter round
/// i2 wave 53, lane L5). The `*0` JNI entry points the wrappers call are
/// `ACC_NATIVE` and stay. Only those three classes reach the table search.
pub(crate) fn is_file_family_shadow(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    matches!(
        class_name,
        "java/io/File" | "java/io/WinNTFileSystem" | "java/io/UnixFileSystem"
    ) && triple_is_retired_shadow(class_name, method_name, descriptor)
}

/// Is `(class, method, descriptor)` `ObjectStreamClass$RecordSupport.
/// deserializationCtr(ObjectStreamClass)`, the one retired row outside
/// `java/lang/invoke/` that hands back a `MethodHandle` carrier
/// (`MH_KIND_RECORD_DESER`, `lang_invoke::native_record_support_deserialization_ctr`)?
/// `--real-jdk` already runs the JDK's body (a retired row of
/// `RETIRED_SHADOW_L4_W19_TRIPLES`, measured by `L4Serial.java`),
/// which builds the handle from `MethodHandles` combinators: exactly what the
/// `jdk-invoke` row lets `--compatible` build too. With the row in force the
/// carrier would be the only VM-minted handle among JDK-built ones, so the
/// stand-in goes with the family (proposal w12-L3 item 4; JIT round 23 wave 1
/// L11). `CRATONVM_COMPAT_RECORD_DESER_CTR_JDK_BODY=0` keeps the stand-in.
/// Cold: registration only.
pub(crate) fn is_record_deser_ctr_shadow(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/io/ObjectStreamClass$RecordSupport"
        && method_name == "deserializationCtr"
        && descriptor == "(Ljava/io/ObjectStreamClass;)Ljava/lang/invoke/MethodHandle;"
        && triple_is_retired_shadow(class_name, method_name, descriptor)
}

/// The `--compatible` HotSpot-alignment row that drops this registration over
/// a real JDK, if any: the one index of the `RETIRED_SHADOW_*` families
/// `--compatible` has adopted, each a row
/// (`docs/feature-designs/native-dispatch-architecture.md`,
/// step 1; interpreter round i2 wave 53, lane L5). `NativeMethodRegistry::
/// register` asks it once per registration, cold, under `Compatible` over a
/// real JDK only, and drops the registration when the row is in the set. A
/// new family is one arm here and one row.
pub(crate) fn compat_row_of_shadow(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<cratonvm_types::HotSpotAlignment> {
    compat_row_of_shadow_with(
        class_name,
        method_name,
        descriptor,
        compat_family_of_class(class_name),
        class_is_under_retired_prefix(class_name),
    )
}

/// [`compat_row_of_shadow`] with its two class-only inputs supplied by the
/// caller: `family` is [`compat_family_of_class`]`(class_name)` and
/// `retired_prefix` is [`class_is_under_retired_prefix`]`(class_name)`. Both
/// are pure functions of the class name and the build's constants, and
/// [`compat_family_of_class`] alone (a prefix walk over every
/// [`COMPAT_FAMILIES`] entry) was most of `NativeMethodRegistry::register`'s
/// cost under `--compatible`; the registry computes them once per class
/// (`ClassFacts`, Phase 5 fast boot, `p5-natreg`). Same answer, same order.
pub(crate) fn compat_row_of_shadow_with(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    family: Option<&'static CompatFamily>,
    retired_prefix: bool,
) -> Option<cratonvm_types::HotSpotAlignment> {
    use cratonvm_types::HotSpotAlignment as Row;
    if is_parallel_capable_loader_shadow(class_name, method_name, descriptor) {
        return Some(Row::ParallelLoaders);
    }
    if is_compat_unmodifiable_view_shadow(class_name, method_name) {
        return Some(Row::JdkUnmodifiableViews);
    }
    if is_url_class_loader_shadow(class_name, method_name, descriptor)
        || is_url_accessor_shadow(class_name, method_name, descriptor)
    {
        return Some(Row::JdkUrlClassLoaders);
    }
    if is_file_family_shadow(class_name, method_name, descriptor) {
        return Some(Row::JdkFileBodies);
    }
    if is_jar_file_family_shadow(class_name, method_name, descriptor) {
        return Some(Row::JdkJarFileBodies);
    }
    // nio21 close: the archive-provider rows (`RETIRED_SHADOW_ARCHIVE_PROVIDER_TRIPLES`)
    // and their mirrored copies on the concrete provider classes, which the
    // retired tables do not list for `--compatible`'s minted providers
    // (`FILE_SYSTEM_PROVIDER_IMPLS`). With them gone, `FileSystems.newFileSystem`
    // offers the path to each installed provider as the JDK's own body does.
    if is_archive_provider_shadow(class_name, method_name, descriptor) {
        return Some(Row::JdkArchiveFs);
    }
    // JIT round 20 wave 14 (lane cjavac): `javac`'s file-manager Bridges
    // (`COMPAT_JAVAC_FILE_MANAGER_TRIPLES`), which `--real-jdk` never lets win
    // over the bytecode.
    if is_javac_file_manager_shadow(class_name, method_name, descriptor) {
        return Some(Row::JdkJavacFileManager);
    }
    // Windows: the producers of the asynchronous FILE channel carrier ride the
    // channel family (see `is_windows_afc_producer_shadow`).
    if is_windows_afc_producer_shadow(class_name, method_name) {
        return Some(Row::JdkAsyncChannels);
    }
    // JIT round 23 wave 1 L11: the record-deserialization constructor handle
    // rides the `jdk-invoke` family (see `is_record_deser_ctr_shadow`).
    if is_record_deser_ctr_shadow(class_name, method_name, descriptor) {
        return Some(Row::JdkInvoke);
    }
    // SBR round 4, lane compat: the families adopted by CLASS, whichever table
    // retired the row (see [`COMPAT_FAMILIES`]).
    if let Some(family) = family {
        if triple_is_retired_shadow_given_prefix(
            || retired_prefix,
            class_name,
            method_name,
            descriptor,
        ) {
            return Some(family.row);
        }
        // nio20 wave 2 (L5; NIO20-L4-3): a family's reviewed non-retired
        // stand-ins over JDK `Code` (see `CompatFamily::bridges`).
        if family
            .bridges
            .iter()
            .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor)
        {
            return Some(family.row);
        }
    }
    // SBR round 5, lane compat2: `System.load` / `System.loadLibrary` are two
    // rows of the System table (`RETIRED_SHADOW_SBR_SYSTEM_TRIPLES`) that belong
    // to the `Runtime` family: their JDK body calls the `Runtime.load0` /
    // `loadLibrary0` bodies the `JdkRuntime` row runs, and the native answered
    // the same questions with other exceptions and messages. So is
    // `BootLoader.loadLibrary`: the real one records the boot loader's own
    // libraries in its `NativeLibraries`, which is what makes a later
    // `System.loadLibrary("zip")` from another loader fail as on HotSpot
    // (`RJdkJni`: "must FAIL for zip once java.util.zip has boot-loaded it").
    // The rest of the System table is not adopted.
    if matches!(
        (class_name, method_name),
        ("java/lang/System", "load" | "loadLibrary")
            | ("jdk/internal/loader/BootLoader", "loadLibrary")
    ) && descriptor == "(Ljava/lang/String;)V"
        && triple_is_retired_shadow(class_name, method_name, descriptor)
    {
        return Some(cratonvm_types::HotSpotAlignment::JdkRuntime);
    }
    // ... and `Reflection.ensureNativeAccess` (a row of the reflection-metadata
    // table): the restricted-method announce `System.load*` / `Runtime.load*` now
    // reach. The native warned for the JDK's own modules (`jdk.net`,
    // `java.management`: the policy it asks lists only the command line's
    // grants) and in its own log format; the real body is
    // `Module.ensureNativeAccess`, silent for a module that starts with native
    // access and HotSpot's `WARNING:` text for the others (strict corpus
    // `JdkOnlyCensusLoadProbe` / `JdkOnlyPlatformProbe`, `real` arm).
    if class_name == "jdk/internal/reflect/Reflection"
        && method_name == "ensureNativeAccess"
        && descriptor == "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/String;Z)V"
        && triple_is_retired_shadow(class_name, method_name, descriptor)
    {
        return Some(cratonvm_types::HotSpotAlignment::JdkRuntime);
    }
    // JIT round 20 (nio20, lane L4; NIO19-6): the two `Unsafe` bulk rows of
    // `RETIRED_SHADOW_L5T_TRIPLES` whose JDK bodies are the argument checks
    // and one call of the `copyMemory0` / `setMemory0` bridge -- the very
    // callback (`native_unsafe_copy_memory_consolidated` /
    // `native_unsafe_set_memory_consolidated`) the stand-in ran. No VM state;
    // what the checks add is HotSpot's `IllegalArgumentException` for a
    // negative size or a non-primitive-array base, which the stand-in copied
    // through. The rest of the `Unsafe` table (the off-heap accessors, the
    // allocator, `objectFieldOffset`, ...) is not adopted: one class, not one
    // family. The `sun/misc/Unsafe` spellings stay too (their JDK 25 bodies
    // call `beforeMemoryAccess()`, a separate decision).
    if is_unsafe_bulk_memory_row(class_name, method_name, descriptor) {
        return Some(cratonvm_types::HotSpotAlignment::JdkUnsafeBulkBodies);
    }
    // JIT round 20 (nio20, lane L4; NIO19-6): four of `java.lang.Object`'s six
    // retired rows (`RETIRED_SHADOW_L2_TRIPLES`). See `is_object_body_row`.
    if is_object_body_row(class_name, method_name, descriptor) {
        return Some(cratonvm_types::HotSpotAlignment::JdkObjectBodies);
    }
    // nio20 wave 2 (L5; NIO20-L4-4): the other two `Object` rows, their own
    // row because they need the generic-reflection carriers gone first
    // (`NativeMethodRegistry::register` holds them while it does not know
    // that). See `is_object_identity_body_row`.
    if is_object_identity_body_row(class_name, method_name, descriptor) {
        return Some(cratonvm_types::HotSpotAlignment::JdkObjectIdentityBodies);
    }
    // nio20 wave 5 (L4): `TimeZone.getDefaultRef()`, the one `TimeZone` row
    // every `Date` field read crosses. See `is_date_default_zone_row`.
    if is_date_default_zone_row(class_name, method_name, descriptor) {
        return Some(cratonvm_types::HotSpotAlignment::JdkDate);
    }
    // JIT round 20 (lane walkcompat): `System.lineSeparator()`, the one row of
    // the `System` table every `PrintWriter.println` crosses. See
    // `is_print_writer_line_separator_row`.
    if is_print_writer_line_separator_row(class_name, method_name, descriptor) {
        return Some(cratonvm_types::HotSpotAlignment::JdkPrintWriter);
    }
    // JIT round 20 wave 5 (lane walkcompat): the `java.sql` date/time family
    // goes with `java.util.Date`'s. See `is_sql_datetime_family_row`.
    if is_sql_datetime_family_row(class_name, method_name, descriptor) {
        return Some(cratonvm_types::HotSpotAlignment::JdkDate);
    }
    // nio21 wave 1 (L4): the three `System` property rows go with the
    // `Properties` / `ConcurrentHashMap` pair (see the predicate).
    if is_chm_pair_system_property_row(class_name, method_name, descriptor) {
        return Some(cratonvm_types::HotSpotAlignment::JdkConcurrentHashMap);
    }
    // nio21 wave 2 (L4): the carrier-thread-local accessors and the `Util`
    // temporary-buffer pool that rides them (see the predicate).
    if is_carrier_thread_local_family_row(class_name, method_name, descriptor) {
        return Some(cratonvm_types::HotSpotAlignment::JdkCarrierThreadLocals);
    }
    // nio21 wave 3 (L1): the three interrupt-status rows of `java.lang.Thread`
    // (see the predicate), not the class.
    if is_thread_interrupt_family_row(class_name, method_name, descriptor) {
        return Some(cratonvm_types::HotSpotAlignment::JdkThreadInterrupt);
    }
    // nio21 wave 3 (L1): the `Unsafe` rows whose JDK body is one call of a
    // kept bridge (see the predicate).
    if is_unsafe_delegation_row(class_name, method_name, descriptor) {
        return Some(cratonvm_types::HotSpotAlignment::JdkUnsafeDelegations);
    }
    // nio21 wave 4 (L1): `SharedSecrets.getJavaLangAccess`, the getter alone
    // (see the predicate).
    if is_shared_secrets_jla_getter_row(class_name, method_name, descriptor) {
        return Some(cratonvm_types::HotSpotAlignment::JdkSharedSecrets);
    }
    // j27 wave 5 (VMALIGN): `VM.initLevel()` / `isBooted()`, the two boot-level
    // getters (see the predicate), not the class.
    if is_vm_init_level_row(class_name, method_name, descriptor) {
        return Some(cratonvm_types::HotSpotAlignment::JdkVmInitLevel);
    }
    None
}

/// Is the triple `jdk.internal.misc.VM.initLevel()I` or `isBooted()Z`, the
/// `JdkVmInitLevel` row (JIT round j27 wave 5, lane VMALIGN)? Retired rows of
/// `RETIRED_SHADOW_SBR_SYSTEM_TRIPLES` / `RETIRED_SHADOW_L5_TRIPLES`,
/// registered under `--compatible` by `native-builtins` `lib.rs` (both) and
/// `phases_early.rs` (`initLevel`); every registration goes. Cold:
/// registration only.
///
/// # The VM-state review
///
/// JDK 25 (`javap -c`, 25.0.3): `initLevel()` is `getstatic initLevel;
/// ireturn` and `isBooted()` is `initLevel >= 4`. The stand-ins answered the
/// PROCESS-wide boot level (`init_level::get_init_level`, floor 2) and a
/// constant `true`; the per-VM static is what HotSpot answers and what the
/// boot now writes on every `--compatible` arm (`Vm::raise_jdk_init_level`:
/// 1 after the `initPhase1` tail, 4 before `main`). `isBooted` is taken only
/// while that 4 is written on the fast arm too
/// (`CRATONVM_COMPAT_FAST_BOOT_JDK_BOOTED`, opt-in since the j27
/// orchestrator found level 4 breaks `StackTraceElement$HashedModules` with no
/// boot layer): with it off (the default) the
/// static stays 1 at `main` and the JDK body would refuse every
/// `ResourcesMgr` / URL-factory caller the stand-in let through.
///
/// NOT taken, each with its blocker:
/// * `isJavaLangInvokeInited` / `setJavaLangInvokeInited`: not a pure read
///   the row needs. Every `--compatible` arm initialises `MethodHandleNatives`
///   before `main` (`vm` `Vm::boot_init_java_lang_invoke`; the fast arm too
///   since j27 wave 8, lane W9BOOT), so the static is already true there; the
///   pinned-`true` stand-in is a `Bridge`, which the JDK's concrete body beats
///   in `resolve_dispatch` anyway. (The earlier note here, "the pinned `true`
///   works" under the fast boot, was wrong: the fast arm left the static
///   false and `MethodHandleAccessorFactory.newFieldAccessor` threw.)
/// * `maxDirectMemory`: `--compatible`'s `saveProperties` native never writes
///   `directMemory`, so the static is the 64 MiB `<clinit>` value, not
///   HotSpot's `Runtime.maxMemory()`; the native answers the VM's own cap.
/// * `getSavedProperty` (throws on a null `savedProps`, a property-store
///   bridge), `saveProperties` (the write bridge), `awaitInitLevel` (held).
pub(crate) fn is_vm_init_level_row(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    if class_name != "jdk/internal/misc/VM" {
        return false;
    }
    let row = match (method_name, descriptor) {
        ("initLevel", "()I") => true,
        ("isBooted", "()Z") => fast_boot_writes_booted_level(
            true,
            true,
        ),
        _ => false,
    };
    row && triple_is_retired_shadow(class_name, method_name, descriptor)
}

/// j27 wave 6 (lane T1LAM2): does the `--compatible` fast boot write level 4
/// before `main`? Only with the level write on AND the boot layer it needs
/// built (`vm` `Vm::boot_publish_fast_boot_jdk_booted` withholds the write
/// without a layer: `StackTraceElement$HashedModules` reads
/// `ModuleLayer.boot()`). Anything less leaves the static at 1, where the JDK's
/// `isBooted` body would answer `false` and HotSpot answers `true`.
pub(in crate::retired_shadow) fn fast_boot_writes_booted_level(jdk_booted: bool, boot_layer: bool) -> bool {
    jdk_booted && boot_layer
}

/// Is the triple `jdk.internal.access.SharedSecrets.getJavaLangAccess()`, the
/// `JdkSharedSecrets` row (nio21 wave 4, L1)? A retired row of the
/// `SharedSecrets` table registered twice under `--compatible`
/// (`shared_secrets_bridge.rs` `FACTORIES` and `lib.rs`
/// `register_t19_h2_shared_secrets_shim`); both go. Cold: registration only.
///
/// # The VM-state review
///
/// The JDK 25 body is `getstatic javaLangAccess; areturn` (`javap -c`,
/// 25.0.3; `SharedSecrets` has no `<clinit>`). The stand-ins answered the
/// per-VM factory memo, a `System$1` carrier; nothing else reads their state.
/// What the body needs is the STATIC, which HotSpot's `initPhase1` writes
/// through `setJavaLangAccess` and `--compatible` never wrote: under the row,
/// `lib.rs` `native_system_init_phase1_real_selected` publishes the same memo
/// object there before and after the body
/// (`shared_secrets_bridge::publish_java_lang_access_static`), the order
/// `--real-jdk` measured (`sun.nio.cs.UTF_8.<clinit>` captures the getter's
/// answer). Every `JavaLangAccess` call still lands on the same `System$1`
/// registrations, so behaviour moves only where HotSpot's does: one object
/// for the life of the VM. NOT taken: the other thirteen getters (their
/// statics are written by `<clinit>`s this VM's stand-ins may skip) and the
/// `jdk/internal/misc` spelling (no such class on 25).
pub(crate) fn is_shared_secrets_jla_getter_row(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "jdk/internal/access/SharedSecrets"
        && method_name == "getJavaLangAccess"
        && descriptor == "()Ljdk/internal/access/JavaLangAccess;"
        && triple_is_retired_shadow(class_name, method_name, descriptor)
}

/// Is the triple one of `java.lang.Thread`'s three interrupt-status rows of
/// `RETIRED_SHADOW_SBR_THREAD_TRIPLES` -- `interrupt()`, `isInterrupted()`,
/// the static `interrupted()` -- the `JdkThreadInterrupt` row (nio21 wave 3,
/// L1)? Cold: registration only.
///
/// # The VM-state review
///
/// The JDK 25 bodies are `interrupted = true; interrupt0()` (plus the
/// `nioBlocker` hand-off under `synchronized (interruptLock)` for another
/// thread), `return interrupted`, and `getAndClearInterrupt()` (`interrupted =
/// false; clearInterruptEvent()`). What they need, and where it comes from in
/// BOTH modes:
///
/// * the `interrupted` FIELD agrees with the VM's own flag: since SBR wave 6
///   every writer of the flag writes the field too --
///   `NativeContextImpl::thread_interrupt` (before raising the flag; it is
///   what `interrupt0` and the JVMTI / JDWP interrupts call),
///   `is_interrupted(true)` (every sleep / wait / join / park consume and
///   `clearInterruptEvent`), both `Object.wait` swaps and the virtual-thread
///   remount (`vm_exec::set_thread_interrupted_field`);
/// * `interrupt0` and `clearInterruptEvent` are genuine `ACC_NATIVE` bridges
///   that stay registered (not retired rows);
/// * `interruptLock` is seeded on every `Thread` the VM builds (`Thread.<init>`
///   natives, the lazy current-thread mirror, `start`), and `nioBlocker` is
///   published by the JDK's `blockedOn` once `jdk-nio-channels` / `jdk-pipe`
///   drop the no-op hook (`is_nio_blocker_hook`), both default on.
///
/// The stand-ins answered `isInterrupted()` from the flag for a started
/// thread and from the field for a NEW one; the field alone is HotSpot's
/// answer in both cases. Residual (both modes, not this row's): a consume that
/// clears the flag, races an `interrupt()` that sets field-then-flag, and
/// writes the field `false` last can leave the field clear with the flag set.
pub(crate) fn is_thread_interrupt_family_row(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/Thread"
        && matches!(
            (method_name, descriptor),
            ("interrupt", "()V") | ("isInterrupted", "()Z") | ("interrupted", "()Z")
        )
        && triple_is_retired_shadow(class_name, method_name, descriptor)
}

/// The `jdk.internal.misc.Unsafe` rows whose JDK 25 body is exactly one call
/// of another `Unsafe` method that both modes keep registered as a `Bridge`
/// (`javap -c -p jdk.internal.misc.Unsafe`, 25.0.3; the callee after `->`).
/// Each is a reviewed entry, not a name pattern: the sub-word CAS family has
/// the same names and a Java body over `objectFieldOffset` arithmetic this VM
/// cannot honour (the `RETIRED_SHADOW_L5_TRIPLES` note), and the
/// `getAndAdd*` / `getAndSet*` rows are loops, not delegations.
pub(in crate::retired_shadow) static UNSAFE_DELEGATION_ROWS: &[(&str, &str)] = &[
    // -> compareAndSetInt
    ("weakCompareAndSetInt", "(Ljava/lang/Object;JII)Z"),
    ("weakCompareAndSetIntAcquire", "(Ljava/lang/Object;JII)Z"),
    ("weakCompareAndSetIntPlain", "(Ljava/lang/Object;JII)Z"),
    ("weakCompareAndSetIntRelease", "(Ljava/lang/Object;JII)Z"),
    // -> compareAndSetLong
    ("weakCompareAndSetLong", "(Ljava/lang/Object;JJJ)Z"),
    ("weakCompareAndSetLongAcquire", "(Ljava/lang/Object;JJJ)Z"),
    ("weakCompareAndSetLongPlain", "(Ljava/lang/Object;JJJ)Z"),
    ("weakCompareAndSetLongRelease", "(Ljava/lang/Object;JJJ)Z"),
    // -> compareAndSetReference
    (
        "weakCompareAndSetReference",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "weakCompareAndSetReferenceAcquire",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "weakCompareAndSetReferencePlain",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "weakCompareAndSetReferenceRelease",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
    ),
    // -> compareAndExchangeInt / Long / Reference
    ("compareAndExchangeIntAcquire", "(Ljava/lang/Object;JII)I"),
    ("compareAndExchangeIntRelease", "(Ljava/lang/Object;JII)I"),
    ("compareAndExchangeLongAcquire", "(Ljava/lang/Object;JJJ)J"),
    ("compareAndExchangeLongRelease", "(Ljava/lang/Object;JJJ)J"),
    (
        "compareAndExchangeReferenceAcquire",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "compareAndExchangeReferenceRelease",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    // -> getIntVolatile / getLongVolatile / getReferenceVolatile
    ("getIntAcquire", "(Ljava/lang/Object;J)I"),
    ("getIntOpaque", "(Ljava/lang/Object;J)I"),
    ("getLongAcquire", "(Ljava/lang/Object;J)J"),
    (
        "getReferenceAcquire",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
    ),
    (
        "getReferenceOpaque",
        "(Ljava/lang/Object;J)Ljava/lang/Object;",
    ),
    // -> putIntVolatile / putLongVolatile / putReferenceVolatile
    ("putIntOpaque", "(Ljava/lang/Object;JI)V"),
    ("putIntRelease", "(Ljava/lang/Object;JI)V"),
    ("putLongRelease", "(Ljava/lang/Object;JJ)V"),
    (
        "putReferenceOpaque",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
    ),
    (
        "putReferenceRelease",
        "(Ljava/lang/Object;JLjava/lang/Object;)V",
    ),
    // -> fullFence (`storeStoreFence` -> `storeFence` -> `fullFence`)
    ("loadFence", "()V"),
    ("storeFence", "()V"),
    ("storeStoreFence", "()V"),
];

/// Is the triple a retired `jdk.internal.misc.Unsafe` row in
/// [`UNSAFE_DELEGATION_ROWS`], the `JdkUnsafeDelegations` row (nio21 wave 3,
/// L1)? The JDK body is one call of a `Bridge` both modes keep
/// (`compareAndSet*`, `compareAndExchange*`, `get*Volatile`, `put*Volatile`,
/// `fullFence`), the very callback family the stand-in ran, so the row moves
/// no VM state: the stand-ins kept none. The `sun/misc/Unsafe` spellings are
/// not taken (their JDK 25 bodies call `beforeMemoryAccess()`, as the
/// `JdkUnsafeBulkBodies` note says). Cold: registration only.
pub(crate) fn is_unsafe_delegation_row(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "jdk/internal/misc/Unsafe"
        && UNSAFE_DELEGATION_ROWS
            .iter()
            .any(|&(m, d)| m == method_name && d == descriptor)
        && triple_is_retired_shadow(class_name, method_name, descriptor)
}

/// Is the triple `java.util.TimeZone.getDefaultRef()`, the one `TimeZone` row
/// the `JdkDate` row takes (nio20 wave 5, L4;
/// `docs/feature-designs/native-dispatch-architecture.md`)?
///
/// `Date.normalize()` calls it on every field read of a fresh `Date`
/// (`getYear`, `toString`, ...), so under `--compatible` each one crossed a
/// native door where `--real-jdk` runs the JDK's static read. The JDK body
/// reads the `defaultTimeZone` static the native reads (and on a cold VM
/// fills it through `setDefaultZone()`: `getSystemTimeZoneID`, a `Bridge`,
/// then `ZoneInfoFile.getZoneInfo`, the native the stand-in's own fallback
/// mirrors), so it carries no state of its own, and the `getDefault` native
/// that stays reads the same static. The other four `TimeZone` rows stay
/// native: `getTimeZone` / `getID` / `getDisplayName` are the `ZoneInfo`
/// carrier model the `RETIRED_SHADOW_L1_DATE_TRIPLES` note holds back. Cold:
/// registration only.
pub(crate) fn is_date_default_zone_row(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/util/TimeZone"
        && method_name == "getDefaultRef"
        && descriptor == "()Ljava/util/TimeZone;"
        && triple_is_retired_shadow(class_name, method_name, descriptor)
}

/// Is the triple `java.lang.System.lineSeparator()`, the one row of
/// `RETIRED_SHADOW_SBR_SYSTEM_TRIPLES` the `JdkPrintWriter` row takes (JIT
/// round 20, lane walkcompat;
/// `docs/feature-designs/native-dispatch-architecture.md`,
/// `CRATONVM_COMPAT_LINE_SEPARATOR_JDK_BODY`, default ON, `=0` keeps the
/// native)?
///
/// `PrintWriter.newLine()` (every `println`) and `BufferedWriter.newLine()`
/// call it once per line. Under `--compatible` it was a native door
/// (`lang_system::native_system_line_separator`: a class lookup and a
/// static-field lookup BY NAME, then the static) that the JIT cannot inline,
/// where `--real-jdk` runs the JDK's `return lineSeparator;` (the SBR system
/// table measured 185-238 ns against 60-73 there). The native answers the
/// same `lineSeparator` static the JDK body reads, which `initPhase1` sets in
/// both modes (`native_system_init_phase1` step 3 / the JDK's own body), so
/// the row carries no state: the answer is the same object either way. The
/// rest of the `System` table stays native under `--compatible` (the property
/// store, `exit`, the `getenv` / `getLogger` costs). Cold: registration only.
pub(crate) fn is_print_writer_line_separator_row(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/System"
        && method_name == "lineSeparator"
        && descriptor == "()Ljava/lang/String;"
        && triple_is_retired_shadow(class_name, method_name, descriptor)
}

/// Is the triple one of the 11 `java.sql` `Date` / `Time` / `Timestamp` rows
/// (`RETIRED_SHADOW_SQL_DATETIME_TRIPLES`) that the `JdkDate` row takes under
/// `--compatible` (JIT round 20 wave 5, lane walkcompat;
/// `docs/feature-designs/native-dispatch-architecture.md`,
/// `CRATONVM_COMPAT_SQL_DATETIME_JDK_BODIES`, default ON, `=0` keeps the stubs)?
///
/// `R16Fieldsite2Layout date` ran 9.7x `--real-jdk` under `--compatible`, and
/// `CRATONVM_COMPAT_ALIGN=all` did not move it: no row covered these stubs
/// (the table's own note: "`--compatible` is unchanged ... no `java/sql/` class
/// is on any `real_protected_stub_class` list"). Every `new Timestamp(long)`
/// ran a native constructor (never an elidable construction) and every
/// `getTime()` -- reached from `hashCode`, `equals`, `compareTo` and
/// `getTime` itself -- a native door, where `--real-jdk` runs the JDK bodies
/// compiled. The table's screen holds for `--compatible` as written: the
/// natives keep no state (they wrote `fastTime` and `nanos` where the real
/// constructors do), no site mints these classes, and the JDK bodies need
/// only `java.util.Date`'s, which the `JdkDate` row already runs. They also
/// answer HotSpot where the natives did not (`setHours` then `getTime`, the
/// table's "HotSpot difference" section). Cold: registration only.
pub(crate) fn is_sql_datetime_family_row(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        class_name,
        "java/sql/Date" | "java/sql/Time" | "java/sql/Timestamp"
    ) && RETIRED_SHADOW_SQL_DATETIME_TRIPLES
        .iter()
        .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor)
}

/// Is the triple one of the three `System` property rows
/// (`RETIRED_SHADOW_SBR5_SYSPROPS_TRIPLES`: `setProperty`, `clearProperty`,
/// `getProperties`) that the `JdkConcurrentHashMap` row leaves to the JDK's
/// bodies with the `Properties` / `ConcurrentHashMap` pair (nio21 wave 1, L4)?
///
/// Their JDK bodies are reads and writes of `System.props`, a `Properties`
/// over the JDK's map. With the pair's natives gone, the stand-ins would
/// keep a side table that the real `System.props` (and every `Properties`
/// body a program runs on it) never sees, which is the split store the
/// PHASE3 retirement exists to avoid; `--real-jdk` has run the three as
/// bytecode since SBR round 5. The rest of the table (`exit`, `runHooks`) is
/// not the pair's. Cold: registration only.
pub(crate) fn is_chm_pair_system_property_row(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/System"
        && matches!(
            method_name,
            "setProperty" | "clearProperty" | "getProperties"
        )
        && triple_is_retired_shadow(class_name, method_name, descriptor)
}

/// Is the triple one of the `java.lang.Object` rows the `JdkObjectBodies` row
/// leaves to the JDK's bodies: `<init>()`, `finalize()`, `wait()`, `wait(J)`?
///
/// None carries VM state. `<init>`'s stand-in is `native_noop_with_this`:
/// a finalizable object is registered at ALLOCATION from its class's
/// finalizer bit (`SharedVm::register_finalizable`, at every allocation site),
/// never by the constructor, so the JDK's empty body loses nothing -- and
/// every constructor chain crossed that door (`R17NioRows heapcb` /
/// `utfencode`: 3 841 / 4 147 crossings per 20 000 ops). `finalize` is an
/// empty body in both. `wait()` is `wait(0L)`; `wait(J)` checks the sign
/// (HotSpot's `IllegalArgumentException("timeout value is negative")`) and
/// calls the `wait0` bridge whose callback the stand-in already was.
///
/// NOT `equals` / `toString`, the other two retired rows: their stand-ins
/// answer for the VM's synthetic generic-reflection carriers structurally and
/// with the type name, so they are the `JdkObjectIdentityBodies` row
/// ([`is_object_identity_body_row`]), which needs those carriers gone. Cold:
/// registration only.
pub(crate) fn is_object_body_row(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    class_name == "java/lang/Object"
        && matches!(
            (method_name, descriptor),
            ("<init>", "()V") | ("finalize", "()V") | ("wait", "()V") | ("wait", "(J)V")
        )
        && triple_is_retired_shadow(class_name, method_name, descriptor)
}

/// Is the triple `java.lang.Object.equals(Object)` or `toString()`, the
/// `JdkObjectIdentityBodies` row (nio20 wave 2, L5; NIO20-L4-4)?
///
/// Their `--compatible` stand-ins answered for the VM's generic-reflection
/// carriers -- objects whose class IS the bare `java.lang.reflect`
/// `ParameterizedType` / `WildcardType` / `GenericArrayType` / `TypeVariable`
/// interface, so `toString` and `equals` resolved up to `Object` -- with the
/// canonical type name and a structural compare. Under
/// `CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS` (default on) `--compatible` builds
/// the JDK's `*Impl` classes for all four wherever `--real-jdk` does
/// (`generics::type_sig_to_java`, the parameterized arm since this row), so
/// what is left of a carrier is what `--real-jdk` hands out too (a type
/// variable with no declaration in scope), and the JDK's identity /
/// `getClass().getName() + "@" + hex(hashCode())` bodies answer as there. For
/// every other receiver the stand-ins already answered the JDK's way: the
/// display class a carrier reports is `Object.getClass`'s (a `Bridge` that
/// stays), arrays and lambdas are named by `Class.getName`. The registry
/// drops the two rows only when the reflect-type switch is on
/// (`NativeMethodRegistry::compat_row_prerequisites_hold`). Cold:
/// registration only.
pub(crate) fn is_object_identity_body_row(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/Object"
        && matches!(
            (method_name, descriptor),
            ("equals", "(Ljava/lang/Object;)Z") | ("toString", "()Ljava/lang/String;")
        )
        && triple_is_retired_shadow(class_name, method_name, descriptor)
}

/// Is the triple one of `jdk.internal.misc.Unsafe`'s two retired bulk rows,
/// `copyMemory(Object,long,Object,long,long)` / `setMemory(Object,long,long,
/// byte)`, the `JdkUnsafeBulkBodies` row? Cold: registration only.
pub(crate) fn is_unsafe_bulk_memory_row(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "jdk/internal/misc/Unsafe"
        && matches!(
            (method_name, descriptor),
            ("copyMemory", "(Ljava/lang/Object;JLjava/lang/Object;JJ)V")
                | ("setMemory", "(Ljava/lang/Object;JJB)V")
        )
        && triple_is_retired_shadow(class_name, method_name, descriptor)
}

/// One family of retired rows that `--compatible` adopts as a unit, and the
/// stand-ins that must go with it.
///
/// # The per-mode retirement policy (SBR round 4, lane compat)
///
/// A `RETIRED_SHADOW_*` table says "the JDK's bytecode is authoritative for
/// this row"; `--real-jdk` enforces that for every table at once by refusing
/// every `SyntheticStub` (a retired `Bridge` is re-tagged one, see
/// `NativeMethodRegistry::register`). `--compatible` registers them all, so a
/// retired row stays the stand-in and the run differs from HotSpot wherever the
/// stand-in does (a bare `java.security.Provider`, a `ForkJoinPool` task run on
/// the caller). Both modes target HotSpot, so the policy is: **a retirement is
/// effective in `--compatible` per FAMILY, once the family's real bodies are
/// measured to run on `--compatible`'s boot**, and a family is one
/// [`HotSpotAlignment`](cratonvm_types::HotSpotAlignment) row (default on from
/// the lane's switch-on commit, `CRATONVM_COMPAT_ALIGN=-<token>` to revert).
///
/// A family is two class-prefix lists, never a list of methods:
///
/// * `classes`: a registration whose class starts with one of these AND whose
///   triple is retired (`triple_is_retired_shadow`, any table, so a row a later
///   wave retires on these classes joins the family without a line here) is not
///   registered under `Compatible` over a real JDK with the row in force.
/// * `stub_classes`: the family's companion stand-ins that were never retired
///   rows (`<clinit>` no-ops, carriers) but that `--real-jdk` refuses as
///   `SyntheticStub` and without which the real bodies cannot run. A
///   registration of one of these with kind `SyntheticStub` is not registered
///   under the row either. `Bridge` and `Intrinsic` rows are never touched, so
///   what stays native in `--real-jdk` stays native here, and by the same
///   rule: this is `--real-jdk`'s stub policy scoped to a family, not a second
///   policy.
pub(crate) struct CompatFamily {
    pub(crate) row: cratonvm_types::HotSpotAlignment,
    pub(crate) classes: &'static [&'static str],
    pub(crate) stub_classes: &'static [&'static str],
    /// Exact triples of the family's classes that are NOT retired rows but
    /// stand in front of a method the JDK declares with `Code`: dropped with
    /// the family, whatever their kind (nio20 wave 2, L5; NIO20-L4-3).
    /// `--real-jdk` needs no such list (concrete bytecode beats a `Bridge` at
    /// dispatch, `resolve_dispatch` step 3), but `--compatible`'s doors are
    /// native-first, so a `Bridge` that only ever served the VM's carriers
    /// would answer a JDK-built receiver of the adopted family. Each entry is
    /// a reviewed decision with its reason beside it, not a prefix.
    pub(crate) bridges: &'static [(&'static str, &'static str, &'static str)],
    /// Class prefixes inside `classes` / `stub_classes` that belong to ANOTHER
    /// family or to a carrier `--compatible` keeps (a prefix here wins).
    pub(crate) except: &'static [&'static str],
}

impl CompatFamily {
    pub(in crate::retired_shadow) fn names_class(&self, prefixes: &[&str], class_name: &str) -> bool {
        prefixes.iter().any(|p| class_name.starts_with(p))
            && !self.except.iter().any(|p| class_name.starts_with(p))
    }
}

