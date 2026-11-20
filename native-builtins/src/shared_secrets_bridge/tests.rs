use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::collections::BTreeSet;

#[test]
fn all_factories_listed() {
    // getJavaObjectInputStreamAccess removed: JDK 25 uses an invokedynamic
    // lambda (no ObjectInputStream$1), so the real getter must run.
    //
    // F17-1 (2026-08-13): 15 -> 14. `getJavaSecurityAccess` was deleted —
    // JEP 486 removed `jdk.internal.access.JavaSecurityAccess` and the
    // getter with it. See the comment at the deletion site in `FACTORIES`.
    assert_eq!(FACTORIES.len(), 14);
}

/// F17-1 (2026-08-13) — REPLACES a guard that could not fail.
///
/// The previous body of this test was, in full:
///
/// ```ignore
/// for (method, ret, _) in FACTORIES {
///     assert!(method.starts_with("getJava"));
///     assert!(method.ends_with("Access"));
///     assert!(ret.contains("Access;"));
/// }
/// ```
///
/// Every name in `FACTORIES` was written to that shape, so the test checked
/// the list against itself: it had no way to distinguish a JDK-true getter
/// from an invented one, and it passed on both `getJavaSecurityAccess` (a
/// method JDK 25 does not declare at all) and `getJavaUtilJarAccess` (a
/// misspelling of `javaUtilJarAccess`) for as long as both were listed.
/// Worse, its `starts_with("getJava")` clause actively *punished* the
/// correct spelling: `javaUtilJarAccess` has no `get` prefix, so repairing
/// the table would have reddened the guard that was supposed to protect it.
///
/// The oracle is now external: the JDK 25 class surface as walked out of the
/// runtime image by `scripts/jdk-baseline/generate.py` and frozen in
/// `scripts/baselines/jdk25-jdk.internal.access.SharedSecrets.tsv`.
///
/// SOUNDNESS OF THIS PARTICULAR ORACLE — **the paragraph that used to be
/// here was true when written and is now false; F33-1 (2026-08-13) replaces
/// it rather than leaving two readings of the same file.**
///
/// It read: *"the generator keeps only rows whose flags contain `public`
/// (`generate.py:175`), so a baseline is blind to package-private, private
/// and `private static native` members … every member of `SharedSecrets` is
/// `public static`, so its 65 baseline rows are its whole surface."* Both
/// halves have moved. The public-only filter was FIXED during this session
/// (F23-1), and the baseline was regenerated: it now carries **101 data rows
/// = 1 CLASS + 1 EXTENDS + 1 SUPERTYPE + 32 FIELD + 66 METHOD**, i.e. the 98
/// members `javap -p` reports, 33 of them `private,static`. The old "65" was
/// the public-method count, and reasoning from it would now under-count the
/// surface by a third.
///
/// The conclusion survives its premise, which is why this is a correction
/// and not a retraction: every *accessor* on `SharedSecrets` really is
/// `public static`, so the factory-name question this test asks is answered
/// the same way by either baseline. Re-measured for this note:
///
/// ```text
/// $ javap -p jdk.internal.access.SharedSecrets | wc -l      # 98 members + wrapper
/// $ javap    jdk.internal.access.SharedSecrets | wc -l      # 65 — the public view
/// ```
///
/// The one *method* plain `javap` hides is `ensureClassInitialized`; the
/// other 32 hidden members are the private static backing fields. **Any
/// claim about a member of this class must be checked at `-p`** — the
/// public view is missing exactly the machinery the four self-initialising
/// getters (`javaUtilJarAccess`, `getJavaNetUriAccess`,
/// `getJavaNetHttpCookieAccess`, `getJavaIORandomAccessFileAccess`) run
/// through, which is what `register_factories`' disposition turns on.
#[test]
fn every_factory_is_declared_by_jdk25_shared_secrets() {
    let baseline = crate::jdk_baseline::parse(crate::jdk_baseline::SHARED_SECRETS);
    for (method, ret, _) in FACTORIES {
        let desc = format!("(){ret}");
        assert!(
            baseline.declares(method, &desc),
            "SharedSecrets bridge registers `{method}{desc}`, which JDK 25's \
             jdk.internal.access.SharedSecrets does not declare. Candidate \
             descriptors the JDK does declare under that name: {:?}. If the \
             list is empty the member is absent outright (delete the entry); \
             if it is non-empty you have the descriptor wrong. Oracle: \
             scripts/baselines/jdk25-jdk.internal.access.SharedSecrets.tsv.",
            baseline.descriptors_named(method),
        );
    }
}

/// Mutation check for the guard above: it must actually reject the two
/// spellings F17-1 removed, not merely accept the ones that survived.
///
/// Without this, `every_factory_is_declared_by_jdk25_shared_secrets` would
/// be one silent `parse()` change away from being as vacuous as the test it
/// replaced (a `Baseline` that parsed to zero rows would make `declares`
/// return `false` for everything — but an empty `FACTORIES` loop would still
/// pass). Asserting a KNOWN-BAD name is rejected and a KNOWN-GOOD one is
/// accepted pins both directions.
#[test]
fn jdk25_baseline_rejects_the_two_spellings_f17_1_removed() {
    let baseline = crate::jdk_baseline::parse(crate::jdk_baseline::SHARED_SECRETS);
    assert!(
        !baseline.declares(
            "getJavaSecurityAccess",
            "()Ljdk/internal/access/JavaSecurityAccess;"
        ),
        "JEP 486 removed JavaSecurityAccess; nothing declares this getter"
    );
    assert!(
        !baseline.declares(
            "getJavaUtilJarAccess",
            "()Ljdk/internal/access/JavaUtilJarAccess;"
        ),
        "the real spelling has never carried the `get` prefix"
    );
    assert!(
        baseline.declares(
            "javaUtilJarAccess",
            "()Ljdk/internal/access/JavaUtilJarAccess;"
        ),
        "…and the corrected spelling must be the one the JDK declares"
    );
}

#[test]
fn owner_classes_are_unique() {
    let mut seen = std::collections::HashSet::new();
    for owner in owner_classes() {
        assert!(seen.insert(owner), "duplicate owner class: {owner}");
    }
    // Every factory has a distinct owner class, so the unique-owner count
    // tracks FACTORIES.len() (14 since F17-1 removed `getJavaSecurityAccess`
    // — see `all_factories_listed`). Derive it so the two stay in
    // lock-step instead of drifting on the next factory add/remove.
    assert_eq!(seen.len(), FACTORIES.len());
}

#[test]
fn all_factory_entry_points_registered() {
    // Instantiate a fresh registry and apply the bridge.
    let mut r = NativeMethodRegistry::new();
    register_wp1_4_shared_secrets(&mut r);
    for (method, ret, _) in FACTORIES {
        let desc = format!("(){}", ret);
        assert!(
            r.find("jdk/internal/access/SharedSecrets", method, &desc)
                .is_some(),
            "factory {method} not registered (descriptor {desc})"
        );
        assert!(
            r.find("jdk/internal/misc/SharedSecrets", method, &desc)
                .is_some(),
            "legacy-package factory {method} not registered"
        );
    }
}

#[test]
fn representative_method_registered_per_owner() {
    // For each of the 14 owner classes, probe for one
    // representative method we know is registered. This is
    // a sanity check that every `register_*` call landed.
    //
    // F17-1 (2026-08-13): 14 here is NOT `FACTORIES.len()` (also 14, by
    // coincidence since F33-1) and must not be re-derived from it — the two
    // lists have never had the same membership. Measured against the current
    // tree, the owner probed below that has NO entry in `FACTORIES` is
    // `java/io/ObjectInputStream$1` (deliberately not intercepted, so the
    // real invokedynamic getter runs — see the note in `FACTORIES`), and the
    // factory owner NOT probed below is `java/io/FileDescriptor$1`. Reading
    // either count as the other is how the two drift with nothing noticing.
    //
    // F33-1 (2026-08-13): 15 -> 14. The `java/security/AccessController$1`
    // probe went with `register_java_security_access`, whose whole family —
    // interface, implementation class and `SharedSecrets` accessor — JEP 486
    // removed. This assertion is the reason that deletion could not be made
    // by the two earlier lanes that tried: it names the triple, so the
    // registrar and the probe have to move in one commit.
    let mut r = NativeMethodRegistry::new();
    register_wp1_4_shared_secrets(&mut r);
    let expected: &[(&str, &str, &str)] = &[
        (
            "java/lang/System$1",
            "currentCarrierThread",
            "()Ljava/lang/Thread;",
        ),
        (
            "java/lang/invoke/MethodHandleImpl$1",
            "makeClassValueMap",
            "()Ljava/util/Map;",
        ),
        (
            "java/lang/ref/Reference$1",
            "waitForReferenceProcessing",
            "()Z",
        ),
        (
            "java/lang/reflect/ReflectAccess",
            "copyMethod",
            "(Ljava/lang/reflect/Method;)Ljava/lang/reflect/Method;",
        ),
        ("java/io/Console$1", "console", "()Ljava/io/Console;"),
        (
            "cratonvm/internal/ss/JavaIORandomAccessFileAccess$1",
            "open",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/io/RandomAccessFile;",
        ),
        (
            "java/net/InetAddress$1",
            "getHostFromNameService",
            "(Ljava/net/InetAddress;Z)Ljava/lang/String;",
        ),
        (
            "cratonvm/internal/ss/JavaNetUriAccess$1",
            "create",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/URI;",
        ),
        // `java/nio/Buffer$2.getBufferPool()` was HERE and is retired
        // (`61a1647aa`). `javap -p` says NO supported JDK 25 image
        // declares it, so the registration could never be dispatched and
        // this row asserted the presence of a dead one.
        (
            "cratonvm/internal/ss/JavaUtilJarAccess$1",
            "jarFileHasClassPathAttribute",
            "(Ljava/util/jar/JarFile;)Z",
        ),
        (
            "java/util/zip/ZipFile$1",
            "entryLocalNameEncoding",
            "(Ljava/util/zip/ZipEntry;)Z",
        ),
        (
            "cratonvm/internal/ss/JavaNetHttpCookieAccess$1",
            "parseCookie",
            "(Ljava/lang/String;)Ljava/util/List;",
        ),
        (
            "java/io/ObjectInputStream$1",
            "checkArray",
            "(Ljava/io/ObjectInputStream;Ljava/lang/Class;I)V",
        ),
        (
            "java/util/ResourceBundle$1",
            "setParent",
            "(Ljava/util/ResourceBundle;Ljava/util/ResourceBundle;)V",
        ),
    ];
    assert_eq!(expected.len(), 13);
    for (owner, method, desc) in expected {
        assert!(
            r.find(owner, method, desc).is_some(),
            "expected method {owner}.{method}{desc} not registered"
        );
    }
}

#[test]
fn java_nio_access_direct_buffer_pool_registered_for_jdk17_shape() {
    // JDK 17's VM$BufferPoolsHolder invokes
    // SharedSecrets.getJavaNioAccess().getDirectBufferPool() through the
    // JavaNioAccess interface. Depending on host JDK shape and CratonVM
    // anonymous-class metadata, the receiver can be Buffer$1, Buffer$2, or
    // reached only via the interface owner.
    let mut r = NativeMethodRegistry::new();
    register_wp1_4_shared_secrets(&mut r);
    let descriptor = "()Ljdk/internal/misc/VM$BufferPool;";

    // RETIRED 2026-08-24 (`61a1647aa`), and the assertion is INVERTED
    // rather than deleted, because the absence is the property now.
    //
    // `javap` decided it: `java.nio.Buffer$2.getDirectBufferPool()` is
    //
    //     0: getstatic  java/nio/Bits.BUFFER_POOL
    //     3: areturn
    //
    // two instructions returning an object real bytecode already builds.
    // The native in front of it existed only to hand back a FABRICATED
    // carrier, and under `--real-jdk` that fabrication is refused from
    // inside `VM$BufferPoolsHolder.<clinit>` — where a refusal is not
    // catchable, poisons `jdk.internal.misc.VM` for the process, and takes
    // the whole platform MBeanServer with it. The interception was strictly
    // worse than nothing, so re-adding it must fail this test.
    for owner in [
        "java/nio/Buffer$1",
        "java/nio/Buffer$2",
        "jdk/internal/access/JavaNioAccess",
    ] {
        assert!(
            r.find(owner, "getDirectBufferPool", descriptor).is_none(),
            "{owner}.getDirectBufferPool is registered again — the real \
             `Bits.BUFFER_POOL` bytecode must serve it, or strict mode \
             loses the platform MBeanServer out of a <clinit>"
        );
    }

    for (method, desc) in [
        ("getName", "()Ljava/lang/String;"),
        ("getCount", "()J"),
        ("getTotalCapacity", "()J"),
        ("getMemoryUsed", "()J"),
    ] {
        assert!(
            r.find("jdk/internal/misc/VM$BufferPool", method, desc)
                .is_some(),
            "VM$BufferPool.{method}{desc} must be registered"
        );
    }
}

#[test]
fn java_lang_access_define_class_registered() {
    // `jdk.internal.reflect.ClassDefiner.defineClass` calls
    // `SharedSecrets.getJavaLangAccess().defineClass(...)`, which
    // dispatches on the `System$1` singleton's concrete class. Missing
    // this registration aborted every Arquillian
    // `testsuite/integration-arquillian/tests/base` test class with
    // NoSuchMethodError during JUnit5 test-plan construction (see
    // docs/feature-designs/native-dispatch-architecture.md).
    let mut r = NativeMethodRegistry::new();
    register_wp1_4_shared_secrets(&mut r);
    assert!(
        r.find(
            "java/lang/System$1",
            "defineClass",
            "(Ljava/lang/ClassLoader;Ljava/lang/String;[BLjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        )
        .is_some(),
        "JavaLangAccess.defineClass not registered on java/lang/System$1"
    );
}

#[test]
fn java_lang_access_find_bootstrap_class_or_null_registered() {
    // `jdk.internal.loader.BootLoader.loadClassOrNull` calls
    // `SharedSecrets.getJavaLangAccess().findBootstrapClassOrNull(name)`,
    // which dispatches on the `System$1` singleton's concrete class.
    // Missing this registration raised NoSuchMethodError on every SSSD
    // Keycloak Arquillian test row (see
    // docs/feature-designs/real-frame-deopt.md).
    let mut r = NativeMethodRegistry::new();
    register_wp1_4_shared_secrets(&mut r);
    assert!(
        r.find(
            "java/lang/System$1",
            "findBootstrapClassOrNull",
            "(Ljava/lang/String;)Ljava/lang/Class;",
        )
        .is_some(),
        "JavaLangAccess.findBootstrapClassOrNull not registered on java/lang/System$1"
    );
}

#[test]
fn java_lang_access_blocked_on_jdk21_overload_registered() {
    let mut r = NativeMethodRegistry::new();
    register_wp1_4_shared_secrets(&mut r);
    let desc = "(Lsun/nio/ch/Interruptible;)V";
    assert!(
        r.find("java/lang/System$1", "blockedOn", desc).is_some(),
        "JavaLangAccess.blockedOn(Interruptible) not registered on java/lang/System$1"
    );
    assert!(
        r.find("jdk/internal/access/JavaLangAccess", "blockedOn", desc)
            .is_some(),
        "JavaLangAccess.blockedOn(Interruptible) not registered on interface fallback"
    );
}

#[test]
fn java_lang_access_carrier_thread_local_registered() {
    let mut r = NativeMethodRegistry::new();
    register_wp1_4_shared_secrets(&mut r);
    for (name, desc) in [
        (
            "getCarrierThreadLocal",
            "(Ljdk/internal/misc/CarrierThreadLocal;)Ljava/lang/Object;",
        ),
        (
            "setCarrierThreadLocal",
            "(Ljdk/internal/misc/CarrierThreadLocal;Ljava/lang/Object;)V",
        ),
        (
            "removeCarrierThreadLocal",
            "(Ljdk/internal/misc/CarrierThreadLocal;)V",
        ),
        (
            "isCarrierThreadLocalPresent",
            "(Ljdk/internal/misc/CarrierThreadLocal;)Z",
        ),
    ] {
        assert!(
            r.find("java/lang/System$1", name, desc).is_some(),
            "JavaLangAccess.{name}{desc} not registered on java/lang/System$1"
        );
        assert!(
            r.find("jdk/internal/access/JavaLangAccess", name, desc)
                .is_some(),
            "JavaLangAccess.{name}{desc} not registered on interface fallback"
        );
    }
}

/// The two candidate names must actually differ, or every test below is
/// vacuous: a one-entry table returns its only entry from both the match
/// arm and the fallback, and nothing distinguishes them.
#[test]
fn the_candidate_table_holds_two_distinct_names() {
    assert_eq!(JLA_CARRIER_CANDIDATES.len(), 2);
    assert_ne!(JLA_CARRIER_CANDIDATES[0], JLA_CARRIER_CANDIDATES[1]);
    assert!(JLA_CARRIER_CANDIDATES.contains(&"java/lang/System$1"));
    assert!(JLA_CARRIER_CANDIDATES.contains(&"java/lang/System$2"));
}

/// The JDK 21 shape, which is the defect this whole change exists for:
/// `System$1` is a `PrivilegedAction` there and answers `false`, and the
/// carrier is the SECOND candidate.
#[test]
fn the_carrier_is_whichever_candidate_the_image_says_implements_the_interface() {
    let mut asked: Vec<String> = Vec::new();
    let picked = pick_jla_carrier(|cand| {
        asked.push(cand.to_string());
        Some(cand == "java/lang/System$2")
    });
    assert_eq!(picked, "java/lang/System$2");
    assert_eq!(asked, ["java/lang/System$1", "java/lang/System$2"]);
}

/// The JDK 25 shape. This is the arm that keeps the test above honest: a
/// rule that simply preferred the last candidate would pass that one and
/// fail this one, and it is also the arm that shows a 25 image pays exactly
/// ONE lookup rather than probing a name that does not exist there.
#[test]
fn the_first_candidate_wins_when_the_image_says_it_is_the_carrier() {
    let mut asked: Vec<String> = Vec::new();
    let picked = pick_jla_carrier(|cand| {
        asked.push(cand.to_string());
        Some(cand == "java/lang/System$1")
    });
    assert_eq!(picked, "java/lang/System$1");
    assert_eq!(
        asked,
        ["java/lang/System$1"],
        "the walk kept probing after the carrier was identified"
    );
}

/// An image that says no to everything, and an image that cannot say
/// anything, are different inputs and must both degrade to the historical
/// pin — not to a panic, and not to an empty owner that would mint the
/// singleton under no class at all.
#[test]
fn an_image_that_identifies_no_carrier_degrades_to_the_historical_pin() {
    assert_eq!(pick_jla_carrier(|_| Some(false)), "java/lang/System$1");
    assert_eq!(pick_jla_carrier(|_| None), "java/lang/System$1");
}

/// `None` is "could not say", not "no". A candidate that cannot be resolved
/// must not stop the walk before the one that can.
#[test]
fn an_unanswerable_candidate_does_not_shadow_the_answerable_one() {
    let picked = pick_jla_carrier(|cand| {
        if cand == "java/lang/System$1" {
            None
        } else {
            Some(true)
        }
    });
    assert_eq!(picked, "java/lang/System$2");
}

/// The registrar spells its two carrier names inline so the drift scanner
/// can resolve them (see the comment at that loop), which puts them out of
/// the compiler's reach as a copy of `JLA_CARRIER_CANDIDATES`. Tie them
/// together here: a name added to the const and not to that loop is a
/// carrier that would be minted and then answer none of the 39 JLA calls
/// registered on the other one.
#[test]
fn the_registrar_covers_every_carrier_candidate() {
    let mut r = NativeMethodRegistry::new();
    register_wp1_4_shared_secrets(&mut r);
    for cand in JLA_CARRIER_CANDIDATES {
        let n = r
            .dump_registrations()
            .into_iter()
            .filter(|(cls, ..)| *cls == cand)
            .count();
        assert!(
            n >= 30,
            "carrier candidate {cand} carries only {n} registrations; the \
             inline list in register_java_lang_access has drifted from \
             JLA_CARRIER_CANDIDATES"
        );
    }
}

#[test]
fn every_jla_carrier_candidate_carries_the_same_registrations() {
    // A JLA native registered on only one candidate name is a native that
    // vanishes on the other image. That is exactly how
    // `parkVirtualThread(long)` and `encodeASCII(char[],int,byte[],int,int)`
    // came back as NoSuchMethodError the first time the strict corpus ran
    // against a 21 image: on 21 the carrier is `System$2`, and every
    // registration named `System$1`.
    //
    // This asserts the property rather than the spelling, so adding a JLA
    // method to one name only is a red here instead of a defect found on
    // whichever JDK CI does not run.
    let mut r = NativeMethodRegistry::new();
    register_wp1_4_shared_secrets(&mut r);

    let per_candidate: Vec<(&str, BTreeSet<(String, String)>)> = JLA_CARRIER_CANDIDATES
        .iter()
        .map(|cand| {
            let set = r
                .dump_registrations()
                .into_iter()
                .filter(|(cls, ..)| cls == cand)
                .map(|(_, name, desc, _)| (name.to_string(), desc.to_string()))
                .collect();
            (*cand, set)
        })
        .collect();

    // Control: two empty sets are equal, and would prove nothing. Measured
    // 39 distinct (name, descriptor) pairs on 2026-09-09; the floor is set
    // below that so a legitimate removal does not red, but a collapse to a
    // handful (the registrar stopped running) still does.
    assert!(
        per_candidate[0].1.len() >= 30,
        "expected the JLA registrar to cover the interface, got {} on {}",
        per_candidate[0].1.len(),
        per_candidate[0].0
    );

    let (base_name, base) = &per_candidate[0];
    for (name, set) in &per_candidate[1..] {
        let missing: Vec<_> = base.difference(set).collect();
        let extra: Vec<_> = set.difference(base).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "JLA registrations differ between {base_name} and {name}:\n  \
             missing from {name}: {missing:?}\n  only on {name}: {extra:?}"
        );
    }
}

#[test]
fn java_lang_access_find_native_registered() {
    // `java.lang.foreign.SymbolLookup.loaderLookup()` calls through the
    // real JDK `JavaLangAccess.findNative(ClassLoader, String)` bridge.
    // The System$1 concrete owner is the normal dispatch target; the
    // interface registration covers fallback invokeinterface resolution.
    let mut r = NativeMethodRegistry::new();
    register_wp1_4_shared_secrets(&mut r);
    let desc = "(Ljava/lang/ClassLoader;Ljava/lang/String;)J";
    assert!(
        r.find("java/lang/System$1", "findNative", desc).is_some(),
        "JavaLangAccess.findNative not registered on java/lang/System$1"
    );
    assert!(
        r.find("jdk/internal/access/JavaLangAccess", "findNative", desc)
            .is_some(),
        "JavaLangAccess.findNative not registered on interface fallback"
    );
}

/// F33-1 (2026-08-13) — **a factory must carry the kind of the owner it
/// hands out, not the kind its own receiver class earns.**
///
/// This is the guard for the capability defect described on
/// [`register_factories`]. It is deliberately an IFF and not a list check:
/// the interesting failure is not "the wrong four are demoted", it is
/// "a factory and its owner disagree", which is what a future author
/// reintroduces by adding a stand-in owner without thinking about the
/// getter that returns it.
///
/// # It is red on the tree this replaced
///
/// Before this change every factory was a plain `register` under the
/// registrar's ambient `Bridge`, so all fourteen came back `Bridge` and the
/// four `cratonvm/internal/ss/…$1` rows failed the `==`. That is the
/// mutation check in the direction that matters, and it is worth being
/// precise that the OTHER direction is also covered: making
/// `register_factories` demote unconditionally reddens the ten real-JDK
/// owners, so "fix it by demoting everything" cannot pass either.
///
/// The frozen four at the end are a second, weaker assertion on the same
/// run. They do not carry the invariant — the loop does — but they name the
/// population, so a change that legitimately shrinks it (retargeting a
/// carrier onto the JDK's own implementation class, e.g.
/// `java/util/jar/JavaUtilJarAccessImpl`) has to say so here.
#[test]
fn factory_kind_follows_the_owner_it_hands_out() {
    use cratonvm_native_api::NativeKind;

    let mut r = NativeMethodRegistry::new();
    register_wp1_4_shared_secrets(&mut r);

    let mut demoted: Vec<&str> = Vec::new();
    for (method, ret, owner) in FACTORIES {
        let desc = format!("(){ret}");
        let kind = r
            .kind_of("jdk/internal/access/SharedSecrets", method, &desc)
            .unwrap_or_else(|| panic!("SharedSecrets.{method}{desc} is not registered at all"));
        let owner_is_dropped = factory_is_orphaned_by_strict_mode(owner);
        // SBR w6 SYSTEM: a factory RETIRED outright is a third legitimate
        // reason for `SyntheticStub` beside a dropped owner. The retired
        // getter is the JDK's own `return theField;`, the field holds the
        // REAL instance of the (kept) owner, and the owner's methods are
        // untouched, so the pair is still admitted together: nothing is
        // handed a carrier with no methods.
        let retired = cratonvm_native_api::retired_shadow::triple_is_retired_shadow(
            "jdk/internal/access/SharedSecrets",
            method,
            &desc,
        );
        assert_eq!(
            kind == NativeKind::SyntheticStub,
            owner_is_dropped || retired,
            "SharedSecrets.{method}{desc} is registered `{}` while its owner \
             `{owner}` is {} by `--real-jdk`. A factory and the receiver it \
             returns must be admitted or refused TOGETHER: a surviving \
             factory over a dropped owner shadows the real JDK getter and \
             hands back a carrier with no methods (`alloc_singleton`'s `Err` \
             arm makes that a wrong-class receiver, not an error), and a \
             refused factory over a live owner removes a working bridge for \
             nothing. Fix `register_factories`, not this test.",
            kind.as_str(),
            if owner_is_dropped { "DROPPED" } else { "kept" },
        );
        if owner_is_dropped {
            demoted.push(*owner);
        }
    }

    demoted.sort_unstable();
    assert_eq!(
        demoted,
        [
            "cratonvm/internal/ss/JavaIORandomAccessFileAccess$1",
            "cratonvm/internal/ss/JavaNetHttpCookieAccess$1",
            "cratonvm/internal/ss/JavaNetUriAccess$1",
            "cratonvm/internal/ss/JavaUtilJarAccess$1",
        ],
        "the set of SharedSecrets factories `--real-jdk` refuses has changed. \
         GROWTH means a new fabricated owner (or a real owner newly added to \
         `NO_IMAGE_JDK_RECEIVERS`) — check that its getter really is better \
         served by the JDK's own bytecode. SHRINKAGE means a carrier was \
         retargeted onto a real class, which is a fix: shrink this list with \
         it and say which class in the commit message. See \
         docs/feature-designs/native-dispatch-architecture.md"
    );
}

/// A `cratonvm/…` owner that nobody listed keeps `Bridge` and re-opens the
/// defect silently.
///
/// [`factory_kind_follows_the_owner_it_hands_out`] cannot catch that on its
/// own: it asks `factory_is_orphaned_by_strict_mode` about the owner, and an
/// unlisted `cratonvm/` name answers `false`, so factory and owner would
/// agree — both `Bridge`, both surviving strict, and the methods on a class
/// no image declares would be dispatching in the mode that exists to refuse
/// exactly that. The IFF is satisfied by the wrong shared answer.
///
/// So the population needs its own statement. Every `cratonvm/`-namespaced
/// owner in [`FACTORIES`] is a stand-in for a JDK shape by construction —
/// it exists only because the bridge had no real class to name — so it
/// belongs in `VM_MINTED_STAND_IN_RECEIVERS`. (The `VM_SERVICE_RECEIVERS`
/// escape hatch does not apply: a reviewed VM service is not something
/// `SharedSecrets` hands to JDK bytecode as a `Java*Access`.)
#[test]
fn every_fabricated_factory_owner_is_a_listed_stand_in() {
    for (method, _, owner) in FACTORIES {
        if !owner.starts_with("cratonvm/") {
            continue;
        }
        assert!(
            cratonvm_native_api::no_image_receiver::VM_MINTED_STAND_IN_RECEIVERS
                .contains(owner),
            "`{owner}` is a fabricated receiver handed out by \
             SharedSecrets.{method}, and it is NOT in \
             `VM_MINTED_STAND_IN_RECEIVERS` \
             (native-api/src/no_image_receiver.rs). Unlisted, its methods \
             keep `NativeKind::Bridge` and dispatch under `--real-jdk` on a \
             class no JDK image declares — and this factory keeps `Bridge` \
             with them, so the two agree and \
             `factory_kind_follows_the_owner_it_hands_out` stays green while \
             the whole family is wrong. Add the name to that table IN SORTED \
             POSITION (it is binary-searched)."
        );
    }
}

/// gc-common w5-d: `JavaLangRefAccess.waitForReferenceProcessing()` answers
/// what the VM's hook answers (it was a constant `false`), and
/// `runFinalization()` reaches the VM's hook (it was a no-op).
#[test]
fn jlra_natives_reach_the_vm_hooks() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let res = jlra_wait_for_reference_processing(&mut ctx, &[]);
    assert!(matches!(res, Ok(Some(Value::Int(0)))), "idle VM: {res:?}");
    ctx.ref_processing_answer.set(true);
    let res = jlra_wait_for_reference_processing(&mut ctx, &[]);
    assert!(
        matches!(res, Ok(Some(Value::Int(1)))),
        "work in flight: {res:?}"
    );
    assert_eq!(ctx.wait_for_reference_processing_calls.get(), 2);

    assert_eq!(ctx.run_finalization_calls.get(), 0);
    let res = jlra_run_finalization(&mut ctx, &[]);
    assert!(matches!(res, Ok(None)), "{res:?}");
    assert_eq!(ctx.run_finalization_calls.get(), 1);
}
