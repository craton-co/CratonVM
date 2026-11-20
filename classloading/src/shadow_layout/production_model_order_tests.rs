use super::*;
use crate::class_manager::synthetic_stub_field_model;

fn instance_names(class: &str) -> Vec<String> {
    synthetic_stub_field_model(class)
        .iter()
        .filter(|f| !f.is_static())
        .map(|f| f.name.to_string())
        .collect()
}

/// The real JDK 21–25 declaration order for each class whose model was
/// rotated. Spelled out so a JDK upgrade falsifies it, and asserted as a
/// PREFIX so a model may stop short of the real field list but must never
/// name a field at an index the image uses for something else.
///
/// Anonymous `_fN` entries are the model declining to make a claim, which
/// is the correct thing for a slot whose meaning differs between the
/// fabricated and real layouts.
#[test]
fn rotated_models_now_name_fields_at_their_real_indices() {
    let cases: &[(&str, &[&str])] = &[
        // AbstractMap contributes keySet, values ahead of k/v.
        (
            "java/util/Collections$SingletonMap",
            &["_f0", "_f1", "k", "v"],
        ),
        // The java.io Reader/Writer chain is spelled differently in the two
        // builds, so it is pinned by `reader_writer_models_match_the_build`
        // below — a row here can only state one of the two.
        //
        // Declaration order, NOT the CodeSource(URL, Certificate[]) ctor.
        (
            "java/security/CodeSource",
            &["location", "signers", "certs"],
        ),
        // Fixed earlier the same day; pinned here so the whole family is
        // covered by one test rather than three.
        (
            "java/security/ProtectionDomain",
            &["codesource", "classloader", "principals", "permissions"],
        ),
        (
            "java/lang/ThreadGroup",
            &["parent", "name", "maxPriority", "daemon"],
        ),
        // AccessibleObject declares TWO fields, and Executable adds two more
        // on top for Method/Constructor. Leaving them out shifted every
        // slot from index 1 down.
        (
            "java/lang/reflect/AccessibleObject",
            &["override", "accessCheckCache"],
        ),
        (
            "java/lang/reflect/Field",
            &[
                "override",
                "accessCheckCache",
                "clazz",
                "slot",
                "name",
                "type",
                "modifiers",
                "trustedFinal",
            ],
        ),
        (
            "java/lang/reflect/Method",
            &[
                "override",
                "accessCheckCache",
                "parameterData",
                "declaredAnnotations",
                "clazz",
                "slot",
                "name",
                "returnType",
                "parameterTypes",
                "exceptionTypes",
                "modifiers",
                // `pad_to(.., 15)` keeps the model's width at
                // `create_method_object`'s legacy floor without naming the
                // nine real fields between `modifiers` and `callerSensitive`.
                "_f11",
                "_f12",
                "_f13",
                "_f14",
            ],
        ),
        // Found 2026-08-05 by running the census under three REAL
        // workloads instead of three probes — the L4 record's own standing
        // caveat. `Logger` has no `level` field at all (the effective level
        // lives inside `config`), so its slot is `_vm12`, anchored PAST the
        // real field count where it lands on padding. `LogManager`'s two
        // such slots were dropped in j27 wave 4 (FS3-4): nothing read them.
        (
            "java/util/logging/Logger",
            &[
                "config",
                "manager",
                "name",
                "loggerBundle",
                "anonymous",
                "catalogRef",
                "catalogName",
                "catalogLocale",
                "parent",
                "kids",
                "callerModuleRef",
                "isSystemLogger",
                "_vm12",
            ],
        ),
        (
            "java/util/logging/LogManager",
            &[
                "props",
                "systemContext",
                "userContext",
                "rootLogger",
                "readPrimordialConfiguration",
                "globalHandlersState",
                "configurationLock",
                "closeOnResetLoggers",
                "listeners",
                "initializedCalled",
                "initializationDone",
                "loggerRefQueue",
            ],
        ),
        (
            "java/lang/reflect/Constructor",
            &[
                "override",
                "accessCheckCache",
                "parameterData",
                "declaredAnnotations",
                "clazz",
                "slot",
                "parameterTypes",
                "exceptionTypes",
                "modifiers",
            ],
        ),
    ];
    // Accumulate rather than assert per case. A `for` loop of `assert_eq!`
    // stops at the first wrong class, so reverting the whole table to the
    // pre-fix models reports ONE row and says nothing about the other eight
    // — which makes the test look far stronger than it is when it is used
    // (as it was) as the non-vacuity check for a family-wide fix.
    let mut wrong: Vec<String> = Vec::new();
    for (class, want) in cases {
        let got = instance_names(class);
        let got: Vec<&str> = got.iter().map(String::as_str).collect();
        if got != *want {
            wrong.push(format!(
                "  {class}\n    model: {got:?}\n    real:  {want:?}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} fabricated models disagree with the real JDK declaration \
         order (javap -p --module java.base <class>):\n{}",
        wrong.len(),
        cases.len(),
        wrong.join("\n")
    );
}

/// The `java.io` Reader/Writer chain is the one family whose model is
/// `#[cfg]`-split, so it needs a per-build assertion rather than a row in
/// the table above.
///
/// The split is load-bearing in BOTH directions and this pins both ends:
///
/// * in the default build the four models must name the real JDK fields and
///   carry NO `_vmN`. A `_vmN` here would be a claim that CratonVM parks a
///   value on a JDK-owned slot, and after `bw_synthetic_fd` went behind the
///   `synthetic-jdk` gate there is no writer and no reader left to do it.
///   Re-introducing one without moving it off slot 0 fails here.
/// * under `synthetic-jdk` slot 0 (and, for `InputStreamReader`, slot 1)
///   must STAY `_vmN`. That is where `native_isr_init`, `native_br_init`,
///   `native_osw_init` and `native_bw_init` park an fd or a wrapped stream,
///   and spelling it anonymously is how `Files.newBufferedWriter`'s fd read
///   as an innocuous `pad` for a day.
///
/// Slots 1 and 2 are the real names in both builds, which is what makes the
/// two lists comparable at a glance.
#[test]
fn reader_writer_models_match_the_build() {
    #[cfg(not(feature = "synthetic-jdk"))]
    let cases: &[(&str, &[&str])] = &[
        ("java/io/InputStreamReader", &["lock", "skipBuffer", "sd"]),
        ("java/io/BufferedReader", &["lock", "skipBuffer", "in"]),
        ("java/io/OutputStreamWriter", &["writeBuffer", "lock", "se"]),
        ("java/io/BufferedWriter", &["writeBuffer", "lock", "out"]),
    ];
    #[cfg(feature = "synthetic-jdk")]
    let cases: &[(&str, &[&str])] = &[
        ("java/io/InputStreamReader", &["_vm0", "_vm1", "sd"]),
        ("java/io/BufferedReader", &["_vm0", "skipBuffer", "in"]),
        ("java/io/OutputStreamWriter", &["_vm0", "lock", "se"]),
        ("java/io/BufferedWriter", &["_vm0", "lock", "out"]),
    ];

    let mut wrong: Vec<String> = Vec::new();
    for (class, want) in cases {
        let got = instance_names(class);
        let got: Vec<&str> = got.iter().map(String::as_str).collect();
        if got != *want {
            wrong.push(format!(
                "  {class}\n    model: {got:?}\n    want:  {want:?}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} java.io Reader/Writer models are wrong for this build \
         (synthetic-jdk = {}):\n{}",
        wrong.len(),
        cases.len(),
        cfg!(feature = "synthetic-jdk"),
        wrong.join("\n")
    );

    // The non-vacuity half: in the default build the claim is specifically
    // that NOTHING is parked, so no `_vmN` may appear anywhere in these four
    // models — including at an index this test's prefix does not reach.
    #[cfg(not(feature = "synthetic-jdk"))]
    for (class, _) in cases {
        let vm: Vec<String> = instance_names(class)
            .into_iter()
            .filter(|n| is_vm_internal_model_field(n))
            .collect();
        assert!(
            vm.is_empty(),
            "{class} still parks {vm:?} on a JDK-owned slot in the default \
             build; move the value or gate its writer on `synthetic-jdk`"
        );
    }
}
